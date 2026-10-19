use proptest::prelude::*;
use types::channel;
use types::name::{self, Name};
use types::sample::Type;
use types::sample::{self, Scalar, Sides};

use super::*;
use crate::hub::client::First;
use crate::hub::client::Request;
use crate::hub::client::text;
use crate::hub::client::{OPEN_BYTES_MAX, PLACES_MAX, Place, Places, place};

fn read(length: u64) -> Read {
    Read {
        length,
        signature: [0x5e; 64],
    }
}

fn open(mode: Mode, patterns: &[&str]) -> Open {
    Open {
        mode,
        select: Selector::new(patterns.iter().copied()).unwrap(),
    }
}

fn pt(key: u128, data_type: &str, name: &str) -> Place {
    Place {
        key: channel::Key::from_u128(key),
        data_type: data_type.parse().unwrap(),
        name: name.parse().unwrap(),
        index: 0,
    }
}

/// `places`, each on the index at place `index`.
fn on<const N: usize>(index: usize, mut places: [Place; N]) -> [Place; N] {
    for place in &mut places {
        place.index = index;
    }
    places
}

fn encode_read(read: Read) -> Vec<u8> {
    let mut out = vec![0xaa; Read::LEN];
    read.encode(&mut out);
    out
}

fn encode_open(open: &Open) -> Vec<u8> {
    let mut out = vec![0xaa; open.encoded_len()];
    open.encode(&mut out);
    out
}

fn encode_places(places: Places) -> Vec<u8> {
    let mut out = vec![0xaa; Places::LEN];
    places.encode(&mut out);
    out
}

fn not_utf8<E>(bytes: &[u8]) -> text::Error<E> {
    text::Error::Utf8(std::str::from_utf8(bytes).unwrap_err())
}

fn unselected(error: name::Error) -> Error {
    Error::Selector(text::Error::Parse(Box::new(error)))
}

/// `places` as one message of the run.
fn encode_run(places: &[Place]) -> Vec<u8> {
    let [message] = place::messages(places, usize::MAX).collect::<Vec<_>>()[..] else {
        panic!("{} places are one message", places.len());
    };
    encode_message(message)
}

fn encode_message(message: place::Message<'_>) -> Vec<u8> {
    let mut out = vec![0xaa; message.encoded_len()];
    message.encode(&mut out);
    out
}

/// The places that `message` encodes.
fn decode_message(message: place::Message<'_>) -> Vec<Place> {
    place::decode(&encode_message(message)).expect("a message of the run decodes")
}

/// The bytes of `place` in a message of the run.
fn place_len(place: &Place) -> usize {
    let [message] = place::messages(std::slice::from_ref(place), usize::MAX)
        .collect::<Vec<_>>()[..]
    else {
        panic!("a place is one message");
    };
    message.encoded_len()
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
    fn the_read() {
        let mut bytes = vec![6, 0x03, 0x02, 0, 0, 0, 0, 0, 0];
        bytes.extend([0x5e; 64]);
        assert_eq!(encode_read(read(0x0203)), bytes);
    }

    #[test]
    fn the_open() {
        let open = open(Mode::Complete, &["site_a.*", "!site_a.x"]);
        let mut bytes = vec![2, 8, 0];
        bytes.extend(b"site_a.*");
        bytes.extend([9, 0]);
        bytes.extend(b"!site_a.x");
        assert_eq!(open.encoded_len(), 22);
        assert_eq!(encode_open(&open), bytes);
        let latest = super::open(Mode::Latest, &["a"]);
        assert_eq!(encode_open(&latest), [1, 1, 0, b'a']);
    }

    #[test]
    fn the_places() {
        assert_eq!(encode_places(Places { count: 3 }), [6, 3, 0, 0, 0]);
    }

    #[test]
    fn the_place() {
        let place = Place {
            index: 0x0304,
            ..pt(0x0102, "f32[3]", "site_a.pt")
        };
        let mut bytes = vec![0x02, 0x01];
        bytes.extend([0; 14]);
        bytes.extend([0x04, 0x03, 0, 0]);
        bytes.push(6);
        bytes.extend(b"f32[3]");
        bytes.push(9);
        bytes.extend(b"site_a.pt");
        assert_eq!(encode_run(&[place]), bytes);
    }
}

mod first {
    use super::*;

    #[test]
    fn decodes_a_request_and_a_read() {
        let request = Request {
            length: 4,
            signature: [0x5e; 64],
        };
        let mut bytes = vec![0; Request::LEN];
        request.encode(&mut bytes);
        assert_eq!(First::decode(&bytes), Ok(First::Request(request)));
        assert_eq!(
            First::decode(&encode_read(read(4))),
            Ok(First::Read(read(4)))
        );
    }

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|kind| ![5, 6, 7].contains(kind)) {
            assert_eq!(
                First::decode(&zeros(kind, Read::LEN)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(First::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn gives_the_error_of_the_kind_that_it_names() {
        assert_eq!(First::decode(&[5]), Err(Error::Length { len: 1 }));
        assert_eq!(First::decode(&[6]), Err(Error::Length { len: 1 }));
        let mut over = encode_read(read(1));
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

mod read {
    use super::*;

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 6) {
            assert_eq!(
                Read::decode(&zeros(kind, Read::LEN)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Read::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 9, 72, 74] {
            assert_eq!(Read::decode(&zeros(6, len)), Err(Error::Length { len }));
        }
    }

    #[test]
    fn checks_the_length_before_the_cap() {
        let mut long = zeros(6, Read::LEN + 1);
        long[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Read::decode(&long), Err(Error::Length { len: 74 }));
    }

    #[test]
    fn takes_an_open_at_the_cap_and_refuses_one_over_it() {
        let at = encode_read(read(OPEN_BYTES_MAX));
        assert_eq!(Read::decode(&at), Ok(read(OPEN_BYTES_MAX)));
        let mut over = at;
        over[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            Read::decode(&over),
            Err(Error::Oversize {
                length: u64::MAX,
                cap: OPEN_BYTES_MAX,
            })
        );
    }

    #[test]
    fn gives_its_body() {
        assert_eq!(read(5).body().remain(), 5);
        assert_eq!(read(0).body().remain(), 0);
    }
}

mod open {
    use super::*;

    #[test]
    fn refuses_an_empty_open() {
        assert_eq!(Open::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_a_mode_that_is_neither_latest_nor_complete() {
        for byte in (0..=u8::MAX).filter(|byte| ![1, 2].contains(byte)) {
            assert_eq!(Open::decode(&[byte, 1, 0, b'a']), Err(Error::Mode { byte }));
        }
    }

    #[test]
    fn refuses_a_pattern_that_ends_past_the_open() {
        assert_eq!(Open::decode(&[1, 1]), Err(Error::Length { len: 2 }));
        assert_eq!(
            Open::decode(&[1, 2, 0, b'a']),
            Err(Error::Length { len: 4 })
        );
    }

    #[test]
    fn refuses_an_open_with_no_include() {
        assert_eq!(Open::decode(&[1]), Err(unselected(name::Error::NoInclude)));
        assert_eq!(
            Open::decode(&[1, 2, 0, b'!', b'a']),
            Err(unselected(name::Error::NoInclude))
        );
    }

    #[test]
    fn refuses_a_pattern_that_does_not_read() {
        assert_eq!(
            Open::decode(&[1, 0, 0]),
            Err(unselected(name::Error::Empty))
        );
        assert_eq!(
            Open::decode(&[1, 3, 0, b'a', b'*', b'b']),
            Err(unselected(name::Error::Wildcard {
                input: "a*b".into()
            }))
        );
        assert_eq!(
            Open::decode(&[1, 1, 0, 0xff]),
            Err(Error::Selector(not_utf8(&[0xff])))
        );
    }

    #[test]
    fn decodes_an_exclusion_as_written() {
        let decoded = Open::decode(&[2, 1, 0, b'a', 2, 0, b'!', b'b']);
        assert_eq!(decoded, Ok(open(Mode::Complete, &["a", "!b"])));
    }
}

mod places {
    use super::*;

    #[test]
    fn decodes_places_of_one_place_and_at_the_cap() {
        for count in [1, 3, PLACES_MAX] {
            let places = Places { count };
            assert_eq!(Places::decode(&encode_places(places)), Ok(places));
        }
    }

    #[test]
    fn refuses_places_of_no_place_or_over_the_cap() {
        for count in [0, PLACES_MAX + 1] {
            let mut bytes = encode_places(Places { count: 1 });
            bytes[1..5].copy_from_slice(&count.to_le_bytes());
            assert_eq!(
                Places::decode(&bytes),
                Err(Error::Places {
                    count,
                    cap: PLACES_MAX,
                })
            );
        }
    }

    #[test]
    #[should_panic(expected = "65537 places are not from 1 to the cap of 65536")]
    fn panics_on_an_encode_over_the_cap() {
        encode_places(Places {
            count: PLACES_MAX + 1,
        });
    }

    #[test]
    #[should_panic(expected = "0 places are not from 1 to the cap of 65536")]
    fn panics_on_an_encode_of_no_place() {
        encode_places(Places { count: 0 });
    }

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 6) {
            assert_eq!(
                Places::decode(&zeros(kind, Places::LEN)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Places::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 4, 6, 9] {
            assert_eq!(Places::decode(&zeros(6, len)), Err(Error::Length { len }));
        }
    }
}

mod run {
    use super::*;

    #[test]
    fn decodes_each_place_of_a_message() {
        let places = [pt(1, "timestamp", "a.time"), pt(2, "list<u8, 16>", "a.b")];
        assert_eq!(place::decode(&encode_run(&places)), Ok(places.to_vec()));
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(place::decode(&[]), Err(Error::Empty));
    }

    /// Each place of `u8` named `a` is 25 bytes.
    #[test]
    fn cuts_a_run_into_messages_of_as_many_whole_places_as_fit() {
        let places = [pt(1, "u8", "a"), pt(2, "u8", "a"), pt(3, "u8", "a")];
        let cut = |max| {
            place::messages(&places, max)
                .map(|message| decode_message(message).len())
                .collect::<Vec<_>>()
        };
        assert_eq!(cut(25), vec![1, 1, 1]);
        assert_eq!(cut(49), vec![1, 1, 1]);
        assert_eq!(cut(50), vec![2, 1]);
        assert_eq!(cut(75), vec![3]);
        assert_eq!(place::messages(&[], 0).count(), 0);
    }

    #[test]
    fn refuses_a_message_that_ends_inside_a_place() {
        let bytes = encode_run(&[pt(1, "u8", "a")]);
        for len in 1..bytes.len() {
            assert_eq!(
                place::decode(&bytes[..len]),
                Err(Error::Length { len }),
                "{len}"
            );
        }
    }

    #[test]
    fn refuses_a_sample_type_or_a_name_that_does_not_read() {
        let cases: [(usize, u8, Error); 4] = [
            (
                21,
                b'x',
                Error::Sample(text::Error::Parse(sample::Error::Syntax)),
            ),
            (21, 0xff, Error::Sample(not_utf8(&[0xff, b'8']))),
            (
                24,
                b'*',
                Error::Name(text::Error::Parse(Box::new(name::Error::Wildcard {
                    input: "*".into(),
                }))),
            ),
            (24, 0xff, Error::Name(not_utf8(&[0xff]))),
        ];
        for (at, byte, error) in cases {
            let mut bytes = encode_run(&[pt(1, "u8", "a")]);
            bytes[at] = byte;
            assert_eq!(place::decode(&bytes), Err(error), "{at} {byte}");
        }
    }

    #[test]
    fn gives_the_first_defect_in_the_order_of_the_bytes() {
        let mut bytes = encode_run(&[pt(1, "u8", "a")]);
        bytes[21] = b'x';
        bytes[23] = 5;
        assert_eq!(
            place::decode(&bytes),
            Err(Error::Sample(text::Error::Parse(sample::Error::Syntax)))
        );
    }

    #[test]
    fn gives_a_bad_sample_type_before_a_missing_name() {
        let mut bytes = encode_run(&[pt(1, "u8", "a")]);
        bytes[21] = b'x';
        bytes.truncate(23);
        assert_eq!(
            place::decode(&bytes),
            Err(Error::Sample(text::Error::Parse(sample::Error::Syntax)))
        );
    }

    #[test]
    fn gives_a_bad_name_before_a_cut_place_after_it() {
        let mut bytes = encode_run(&[pt(1, "u8", "a")]);
        bytes[24] = 0xff;
        bytes.extend_from_slice(&[0; 3]);
        assert_eq!(place::decode(&bytes), Err(Error::Name(not_utf8(&[0xff]))));
    }
}

mod reader {
    use super::*;
    use crate::hub::tests::encode_reply;
    use crate::hub::{self, FromHome, Head, Reply};
    use types::frame::{Path, Range};

    const OPENED: u8 = 1;

    fn opened_at(live: u64) -> Vec<u8> {
        encode_reply(Reply::Opened { live })
    }

    /// A place of `u8` named `a`: 25 bytes.
    fn byte(key: u128) -> Place {
        pt(key, "u8", "a")
    }

    /// A place of `timestamp` named `a`: 32 bytes.
    fn stamp(key: u128) -> Place {
        pt(key, "timestamp", "a")
    }

    /// `places` cut into messages of the run of at most `max` bytes.
    fn messages(places: &[Place], max: usize) -> Vec<Vec<u8>> {
        place::messages(places, max).map(encode_message).collect()
    }

    fn head(series: u32) -> Vec<u8> {
        encode_reply(Reply::Head(Head {
            path: Path::Live,
            range: Range { seq: 0, count: 1 },
            series,
        }))
    }

    /// A reader of `mode` that took `places`.
    fn in_run(mode: Mode, places: Places) -> Reader {
        let mut reader = Reader::new(mode);
        reader
            .decode(Some(&encode_places(places)))
            .expect("the places decode");
        reader
    }

    /// The decoder of the replies that `reader` gives with the last place, in
    /// `message`.
    fn last(reader: &mut Reader, message: &[u8]) -> hub::Reader {
        match reader.decode(Some(message)) {
            Ok(Some(start)) => start.home,
            other => panic!("the last place gave {other:?}"),
        }
    }

    /// The decoder of the replies after `places` and the whole run of `run`, for a
    /// read of `mode`.
    fn after_run(mode: Mode, places: Places, run: &[Place]) -> hub::Reader {
        last(&mut in_run(mode, places), &encode_run(run))
    }

    #[derive(Debug, PartialEq)]
    enum Event {
        More,
        Start(Vec<Place>),
    }

    /// The event of `decoded`, and the decoder of the replies that the last place
    /// gives. Checks that the key set of a start holds its places in order.
    fn split(decoded: Option<Start>) -> (Event, Option<hub::Reader>) {
        let Some(Start { places, set, home }) = decoded else {
            return (Event::More, None);
        };
        let keys = set.entries().iter().map(|entry| entry.key);
        assert!(keys.eq(places.iter().map(|place| place.key)), "{set:?}");
        (Event::Start(places), Some(home))
    }

    fn event(reader: &mut Reader, message: &[u8]) -> Result<Event, Error> {
        reader.decode(Some(message)).map(|decoded| split(decoded).0)
    }

    #[derive(Debug, PartialEq)]
    enum Home {
        Opened(u64),
        Head(u32),
    }

    fn reply(home: &mut hub::Reader, message: &[u8]) -> Result<Home, hub::Error> {
        home.decode(Some(message)).map(|event| match event {
            FromHome::Opened { live } => Home::Opened(live),
            FromHome::Head(head) => Home::Head(head.series),
            other => panic!("no test sends {other:?}"),
        })
    }

    #[test]
    fn decodes_the_places_then_the_run_then_gives_the_replies() {
        let places = Places { count: 3 };
        let run = on(1, [byte(1), stamp(2), byte(3)]);
        let messages = messages(&run, 57);
        assert_eq!(messages.len(), 2);
        let mut reader = Reader::new(Mode::Complete);
        assert_eq!(event(&mut reader, &encode_places(places)), Ok(Event::More));
        assert_eq!(event(&mut reader, &messages[0]), Ok(Event::More));
        let Ok(Some(Start {
            places,
            set,
            mut home,
        })) = reader.decode(Some(&messages[1]))
        else {
            panic!("the last place gave no start");
        };
        assert_eq!(places, run);
        let entries: Vec<_> = set
            .entries()
            .iter()
            .map(|entry| (entry.key, entry.data_type, entry.slot.get()))
            .collect();
        let expected: Vec<_> = run
            .iter()
            .zip(0..)
            .map(|(place, slot)| (place.key, place.data_type, slot))
            .collect();
        assert_eq!(entries, expected);
        assert_eq!(set.groups(), [1]);
        assert_eq!(reply(&mut home, &opened_at(0)), Ok(Home::Opened(0)));
        assert_eq!(reply(&mut home, &head(3)), Ok(Home::Head(3)));
    }

    #[test]
    fn decodes_the_first_message_as_places() {
        let mut reader = Reader::new(Mode::Latest);
        assert_eq!(reader.decode(Some(&[])).err(), Some(Error::Empty));
        assert_eq!(
            reader.decode(Some(&[OPENED])).err(),
            Some(Error::Kind { kind: OPENED })
        );
        assert_eq!(
            reader.decode(Some(&encode_run(&[stamp(2)]))).err(),
            Some(Error::Kind { kind: 2 })
        );
        assert_eq!(
            reader.decode(Some(&[6])).err(),
            Some(Error::Length { len: 1 })
        );
        assert_eq!(
            reader.decode(Some(&zeros(6, Places::LEN))).err(),
            Some(Error::Places {
                count: 0,
                cap: PLACES_MAX
            })
        );
        let places = Places { count: 1 };
        assert_eq!(event(&mut reader, &encode_places(places)), Ok(Event::More));
    }

    #[test]
    fn reads_a_second_places_or_a_reply_in_the_run_as_run_bytes() {
        let places = Places { count: 1 };
        let mut reader = in_run(Mode::Complete, places);
        assert_eq!(
            reader.decode(Some(&encode_places(places))).err(),
            Some(Error::Length { len: 5 })
        );
        assert_eq!(
            reader.decode(Some(&[OPENED])).err(),
            Some(Error::Length { len: 1 })
        );
        assert_eq!(reader.decode(Some(&[])).err(), Some(Error::Empty));
        assert_eq!(
            event(&mut reader, &encode_run(&[stamp(1)])),
            Ok(Event::Start(vec![stamp(1)]))
        );
    }

    #[test]
    fn refuses_a_message_with_more_places_than_remain() {
        let places = Places { count: 2 };
        let mut reader = in_run(Mode::Complete, places);
        let three = [stamp(1), byte(2), byte(3)];
        assert_eq!(
            reader.decode(Some(&encode_run(&three))).err(),
            Some(Error::Run {
                items: 3,
                remain: 2
            })
        );
        assert_eq!(
            event(&mut reader, &encode_run(&three[..1])),
            Ok(Event::More)
        );
        assert_eq!(
            reader.decode(Some(&encode_run(&three[1..]))).err(),
            Some(Error::Run {
                items: 2,
                remain: 1
            })
        );
        assert_eq!(
            event(&mut reader, &encode_run(&three[1..2])),
            Ok(Event::Start(vec![stamp(1), byte(2)]))
        );
    }

    #[test]
    fn checks_the_count_before_the_index_place() {
        let places = Places { count: 1 };
        let mut reader = in_run(Mode::Complete, places);
        assert_eq!(
            reader.decode(Some(&encode_run(&[byte(1), byte(2)]))).err(),
            Some(Error::Run {
                items: 2,
                remain: 1
            })
        );
    }

    /// A run of 4 places of `u8` in 2 messages of 2, on the index at each place: the
    /// first message decodes, and the last gives `Unstamped`. The run stays as it was,
    /// so a last message with a stamp at the index then gives the start.
    #[test]
    fn refuses_an_index_place_that_is_not_a_stamp_at_the_last_message() {
        for index in 0..4 {
            let run = on(index, [byte(1), byte(2), byte(3), byte(4)]);
            let messages = messages(&run, 50);
            assert_eq!(messages.len(), 2);
            let mut reader = in_run(Mode::Complete, Places { count: 4 });
            assert_eq!(event(&mut reader, &messages[0]), Ok(Event::More), "{index}");
            let unstamped = Error::Unstamped {
                place: u32::try_from(index).unwrap(),
                sample: "u8".parse().unwrap(),
            };
            assert_eq!(
                reader.decode(Some(&messages[1])).err(),
                Some(unstamped),
                "{index}"
            );
            assert_eq!(
                reader.decode(Some(&[OPENED])).err(),
                Some(Error::Length { len: 1 }),
                "the reader is still in the run"
            );
            if let Some(at) = index.checked_sub(2) {
                let mut fixed = run.clone();
                fixed[index].data_type = Type::Scalar(Scalar::Stamp);
                let mut end = run[2..].to_vec();
                end[at].data_type = Type::Scalar(Scalar::Stamp);
                assert_eq!(
                    event(&mut reader, &encode_run(&end)),
                    Ok(Event::Start(fixed.to_vec())),
                    "{index}"
                );
            }
        }
    }

    /// Each run of 2 places gives the error of its first place, in place order, whose
    /// index is not an index place, before `Unstamped` of a later place and before
    /// `Repeated`. Each refusal leaves the run as it was.
    #[test]
    fn refuses_an_index_that_is_not_an_index_place_at_the_last_message() {
        let at = |index, place| Place { index, ..place };
        let index = |place, index| Error::Index { place, index };
        let max = usize::try_from(u32::MAX).unwrap();
        let unstamped = Error::Unstamped {
            place: 1,
            sample: "u8".parse().unwrap(),
        };
        let cases = [
            ([stamp(1), at(2, byte(2))], index(1, 2)),
            ([stamp(1), at(max, byte(2))], index(1, u32::MAX)),
            ([at(1, byte(1)), byte(2)], index(0, 1)),
            ([at(5, stamp(1)), at(1, byte(2))], index(0, 5)),
            ([stamp(1), at(7, stamp(1))], index(1, 7)),
            ([stamp(1), at(1, byte(2))], unstamped),
        ];
        let mut reader = in_run(Mode::Complete, Places { count: 2 });
        for (run, error) in cases {
            assert_eq!(
                reader.decode(Some(&encode_run(&run))).err(),
                Some(error),
                "{run:?}"
            );
        }
        assert_eq!(
            event(&mut reader, &encode_run(&[stamp(1), byte(2)])),
            Ok(Event::Start(vec![stamp(1), byte(2)]))
        );
    }

    /// A read has one index. Each run of 2 places gives `Indexes` for its first place,
    /// in place order, whose index is not that of place 0, after the errors of an
    /// earlier place and before `Repeated`. Each refusal leaves the run as it was, and
    /// places that all name a later index give the start.
    #[test]
    fn refuses_places_of_two_indexes_at_the_last_message() {
        let at = |index, place| Place { index, ..place };
        let unstamped = Error::Unstamped {
            place: 0,
            sample: "u8".parse().unwrap(),
        };
        let cases = [
            (
                [stamp(1), at(1, stamp(2))],
                Error::Indexes { place: 1, index: 1 },
            ),
            (
                [stamp(1), at(1, stamp(1))],
                Error::Indexes { place: 1, index: 1 },
            ),
            ([byte(1), at(1, stamp(2))], unstamped),
        ];
        let mut reader = in_run(Mode::Complete, Places { count: 2 });
        for (run, error) in cases {
            assert_eq!(
                reader.decode(Some(&encode_run(&run))).err(),
                Some(error),
                "{run:?}"
            );
        }
        let run = [at(1, byte(1)), at(1, stamp(2))];
        assert_eq!(
            event(&mut reader, &encode_run(&run)),
            Ok(Event::Start(run.to_vec()))
        );
    }

    /// The messages of a fuzz input, as `fuzz::messages` cuts them: each a length byte
    /// and then that many bytes.
    fn input(mut bytes: &[u8]) -> Vec<&[u8]> {
        let mut messages = Vec::new();
        while let Some((&len, rest)) = bytes.split_first() {
            let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
            messages.push(message);
            bytes = rest;
        }
        messages
    }

    /// The bytes of the input `$file` of the fuzz target `wire_hub_client`.
    macro_rules! corpus {
        ($file:literal) => {
            include_bytes!(concat!(
                "../../../../../../oracles/fuzz/wire_hub_client/",
                $file
            ))
        };
    }

    /// What a message of a read stream gives: an event of the reader, or a reply after
    /// the last place.
    #[derive(Debug, PartialEq)]
    enum Step {
        Event(Result<Event, Error>),
        Reply(Result<Home, hub::Error>),
    }

    /// The step of each message of the fuzz input `bytes`, at a reader of a complete
    /// read.
    fn steps(bytes: &[u8]) -> Vec<Step> {
        let mut reader = Reader::new(Mode::Complete);
        let mut home = None;
        let mut steps = Vec::new();
        for message in input(bytes) {
            if let Some(home) = &mut home {
                steps.push(Step::Reply(reply(home, message)));
                continue;
            }
            let event = reader.decode(Some(message)).map(|decoded| {
                let (event, given) = split(decoded);
                home = given;
                event
            });
            steps.push(Step::Event(event));
        }
        steps
    }

    /// The step of a message before the last place.
    fn more() -> Step {
        Step::Event(Ok(Event::More))
    }

    fn refused(error: Error) -> Step {
        Step::Event(Err(error))
    }

    /// Each input `reader_*` of the fuzz target that reaches the last place gives the
    /// step of each of its messages.
    #[test]
    fn reads_each_run_input_of_the_fuzz_target_to_its_last_place() {
        let cases: [(&[u8], Vec<Step>); 5] = [
            (
                corpus!("reader_behind_indexed"),
                vec![
                    more(),
                    Step::Event(Ok(Event::Start(vec![stamp(1), pt(2, "f64", "b")]))),
                    Step::Reply(Err(hub::Error::Length { len: 1 })),
                    Step::Reply(Err(hub::Error::Kind { kind: 3 })),
                ],
            ),
            (
                corpus!("reader_place_indexed"),
                vec![
                    more(),
                    Step::Event(Ok(Event::Start(vec![
                        pt(1, "timestamp", "a.time"),
                        pt(2, "f32[3]", "a.b"),
                    ]))),
                ],
            ),
            (
                corpus!("reader_run_indexed"),
                vec![
                    more(),
                    more(),
                    Step::Event(Ok(Event::Start(
                        on(1, [byte(1), pt(2, "timestamp", "a.time")]).to_vec(),
                    ))),
                    Step::Reply(Err(hub::Error::Length { len: 1 })),
                    Step::Reply(Err(hub::Error::Unopened { kind: 2 })),
                ],
            ),
            (
                corpus!("reader_behind_live_indexed"),
                vec![
                    more(),
                    Step::Event(Ok(Event::Start(vec![stamp(1), pt(2, "f64", "b")]))),
                    Step::Reply(Ok(Home::Opened(0))),
                    Step::Reply(Err(hub::Error::Kind { kind: 3 })),
                ],
            ),
            (
                corpus!("reader_run_live_indexed"),
                vec![
                    more(),
                    more(),
                    Step::Event(Ok(Event::Start(
                        on(1, [byte(1), pt(2, "timestamp", "a.time")]).to_vec(),
                    ))),
                    Step::Reply(Ok(Home::Opened(0))),
                    Step::Reply(Ok(Home::Head(2))),
                ],
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(steps(bytes), expected, "{bytes:?}");
        }
    }

    /// The input `reader_repeated_last` of the fuzz target reads its valid last
    /// message after the refusal of a repeat in the one before it.
    #[test]
    fn reads_the_input_with_a_last_place_after_a_repeat() {
        assert_eq!(
            steps(corpus!("reader_repeated_last_indexed")),
            vec![
                more(),
                refused(Error::Repeated {
                    key: channel::Key::from_u128(1),
                }),
                Step::Event(Ok(Event::Start(vec![
                    pt(1, "timestamp", "a.time"),
                    pt(2, "f64", "b"),
                ]))),
            ]
        );
    }

    /// The input `places_over` of the fuzz target gives `Error::Places`.
    #[test]
    fn refuses_the_places_over_input_of_the_fuzz_target() {
        assert_eq!(
            steps(corpus!("places_over_indexed")),
            [refused(Error::Places {
                count: PLACES_MAX + 1,
                cap: PLACES_MAX,
            })]
        );
    }

    /// Each other input `reader_*` of the fuzz target gives the step of each of its
    /// messages, the last a refusal.
    #[test]
    fn refuses_each_other_run_input_of_the_fuzz_target() {
        let cases: [(&[u8], Error); 11] = [
            (
                corpus!("reader_index_indexed"),
                Error::Index { place: 0, index: 1 },
            ),
            (
                corpus!("reader_index_past_indexed"),
                Error::Index { place: 0, index: 5 },
            ),
            (
                corpus!("reader_groups_indexed"),
                Error::Indexes { place: 2, index: 2 },
            ),
            (
                corpus!("reader_place_cut_indexed"),
                Error::Length { len: 25 },
            ),
            (
                corpus!("reader_place_name_indexed"),
                Error::Name(text::Error::Parse(Box::new(name::Error::Segment {
                    input: "!".into(),
                    segment: "!".into(),
                }))),
            ),
            (
                corpus!("reader_place_name_text_indexed"),
                Error::Name(not_utf8(&[0xff])),
            ),
            (
                corpus!("reader_place_text_indexed"),
                Error::Sample(not_utf8(&[0xff])),
            ),
            (
                corpus!("reader_place_type_indexed"),
                Error::Sample(text::Error::Parse(sample::Error::Syntax)),
            ),
            (
                corpus!("reader_repeated_indexed"),
                Error::Repeated {
                    key: channel::Key::from_u128(1),
                },
            ),
            (corpus!("reader_run_empty_indexed"), Error::Empty),
            (
                corpus!("reader_run_over_indexed"),
                Error::Run {
                    items: 2,
                    remain: 1,
                },
            ),
        ];
        for (bytes, error) in cases {
            assert_eq!(steps(bytes), [more(), refused(error)], "{bytes:?}");
        }
        assert_eq!(
            steps(corpus!("reader_unstamped_indexed")),
            [
                more(),
                more(),
                refused(Error::Unstamped {
                    place: 1,
                    sample: "u8".parse().unwrap(),
                }),
            ]
        );
    }

    /// Each input of the fuzz target with a `Places` of 9 bytes, from before a place
    /// held its index, is refused at its first message.
    #[test]
    fn refuses_each_input_with_the_places_of_9_bytes() {
        let inputs: [&[u8]; 16] = [
            corpus!("places_over"),
            corpus!("reader_behind"),
            corpus!("reader_behind_live"),
            corpus!("reader_place"),
            corpus!("reader_place_cut"),
            corpus!("reader_place_name"),
            corpus!("reader_place_name_text"),
            corpus!("reader_place_text"),
            corpus!("reader_place_type"),
            corpus!("reader_repeated"),
            corpus!("reader_repeated_last"),
            corpus!("reader_run"),
            corpus!("reader_run_empty"),
            corpus!("reader_run_live"),
            corpus!("reader_run_over"),
            corpus!("reader_unstamped"),
        ];
        for bytes in inputs {
            assert_eq!(
                steps(bytes).first(),
                Some(&refused(Error::Length { len: 9 })),
                "{bytes:?}"
            );
        }
    }

    /// After the last place, the session reads a `Places` or a run message as a reply.
    #[test]
    fn refuses_places_or_a_run_after_the_last_place() {
        let places = Places { count: 1 };
        let mut home = after_run(Mode::Complete, places, &[stamp(1)]);
        assert_eq!(
            home.decode(Some(&encode_places(places))).err(),
            Some(hub::Error::Kind { kind: 6 })
        );
        assert_eq!(
            home.decode(Some(&encode_run(&[stamp(7)]))).err(),
            Some(hub::Error::Kind { kind: 7 })
        );
        assert_eq!(reply(&mut home, &opened_at(0)), Ok(Home::Opened(0)));
    }

    #[test]
    #[should_panic(expected = "a stream is decoded after the decoder gave its start")]
    fn panics_on_a_decode_after_the_last_place() {
        let places = Places { count: 1 };
        let mut reader = in_run(Mode::Complete, places);
        last(&mut reader, &encode_run(&[stamp(1)]));
        drop(reader.decode(Some(&[OPENED])));
    }

    #[test]
    #[should_panic(expected = "a stream is decoded after the decoder gave its start")]
    fn panics_on_the_end_after_the_last_place() {
        let mut reader = in_run(Mode::Complete, Places { count: 1 });
        last(&mut reader, &encode_run(&[stamp(1)]));
        drop(reader.decode(None));
    }

    /// The end of the stream before the last place, at the places and in the run,
    /// leaves the decoder as it was.
    #[test]
    fn refuses_the_end_before_the_last_place() {
        let mut reader = Reader::new(Mode::Complete);
        assert_eq!(reader.decode(None).err(), Some(Error::Finished));
        let places = Places { count: 2 };
        assert_eq!(event(&mut reader, &encode_places(places)), Ok(Event::More));
        assert_eq!(
            event(&mut reader, &encode_run(&[stamp(1)])),
            Ok(Event::More)
        );
        let error = reader.decode(None).expect_err("the end");
        assert_eq!(error, Error::Finished);
        assert_eq!(
            error.to_string(),
            "the node finished the stream before its last place"
        );
        assert_eq!(
            event(&mut reader, &encode_run(&[byte(2)])),
            Ok(Event::Start(vec![stamp(1), byte(2)]))
        );
    }

    /// A refused repeat keeps neither the places of its message nor their count,
    /// so a later last message gives the start.
    #[test]
    fn leaves_the_decoder_as_it_was_after_a_repeat() {
        let mut reader = in_run(Mode::Complete, Places { count: 4 });
        assert_eq!(
            event(&mut reader, &encode_run(&[stamp(1), byte(2)])),
            Ok(Event::More)
        );
        assert_eq!(
            reader.decode(Some(&encode_run(&[byte(3), byte(2)]))).err(),
            Some(Error::Repeated {
                key: channel::Key::from_u128(2)
            })
        );
        assert_eq!(
            event(&mut reader, &encode_run(&[byte(3), byte(4)])),
            Ok(Event::Start(vec![stamp(1), byte(2), byte(3), byte(4)]))
        );
    }

    #[test]
    fn takes_an_index_place_of_a_stamp_at_each_place() {
        for index in 0..4 {
            let mut run = on(index, [byte(1), byte(2), byte(3), byte(4)]);
            run[index].data_type = Type::Scalar(Scalar::Stamp);
            let mut reader = in_run(Mode::Complete, Places { count: 4 });
            let messages = messages(&run, 57);
            let (end, before) = messages.split_last().unwrap();
            for message in before {
                assert!(event(&mut reader, message) == Ok(Event::More), "{index}");
            }
            let mut home = last(&mut reader, end);
            assert_eq!(reply(&mut home, &opened_at(0)), Ok(Home::Opened(0)));
        }
    }

    /// A channel in two messages of the run: the first decodes, and the last gives
    /// the least repeated key.
    #[test]
    fn refuses_a_repeated_channel_at_the_last_place() {
        let run = [stamp(3), byte(2), byte(3), byte(2)];
        let messages = messages(&run, 57);
        assert_eq!(messages.len(), 2);
        let mut reader = in_run(Mode::Complete, Places { count: 4 });
        assert_eq!(event(&mut reader, &messages[0]), Ok(Event::More));
        let error = reader.decode(Some(&messages[1])).expect_err("a repeat");
        assert_eq!(
            error,
            Error::Repeated {
                key: channel::Key::from_u128(2)
            }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the places name channel {} more than once",
                channel::Key::from_u128(2)
            )
        );
    }

    #[test]
    fn refuses_an_array_of_stamps_as_the_index() {
        let mut reader = in_run(Mode::Complete, Places { count: 1 });
        let error = reader
            .decode(Some(&encode_run(&[pt(1, "timestamp[2]", "a")])))
            .expect_err("an array is not a stamp");
        assert_eq!(
            error,
            Error::Unstamped {
                place: 0,
                sample: "timestamp[2]".parse().unwrap()
            }
        );
        assert_eq!(
            error.to_string(),
            "the index place 0 has sample type timestamp[2], not timestamp"
        );
    }

    #[test]
    fn opens_a_session_of_the_count_of_places() {
        let mut home = after_run(
            Mode::Complete,
            Places { count: 2 },
            &on(1, [byte(1), stamp(2)]),
        );
        home.decode(Some(&opened_at(0))).expect("the session opens");
        assert_eq!(
            home.decode(Some(&head(3))).err(),
            Some(hub::Error::Places {
                series: 3,
                places: 2
            })
        );
        assert_eq!(reply(&mut home, &head(2)), Ok(Home::Head(2)));
    }

    /// The seqs that a session of `mode`, opened at 5, lost before a first frame at 9.
    fn lost_before_a_frame(mode: Mode) -> std::ops::Range<u64> {
        let mut reader = in_run(mode, Places { count: 1 });
        let Ok(Some(start)) = reader.decode(Some(&encode_run(&[stamp(1)]))) else {
            panic!("the run gives a start");
        };
        let mut frames = hub::Frames::open(start.home, start.set, Some(&opened_at(5)))
            .expect("the session opens");
        let head = Head {
            path: Path::Live,
            range: Range { seq: 9, count: 2 },
            series: 1,
        };
        frames
            .decode(Some(&encode_reply(Reply::Head(head))))
            .expect("a head");
        frames
            .decode(Some(&crate::hub::tests::encode_ends(&[(0, 0)])))
            .expect("ends");
        let Ok(hub::Due::Draft(layout)) = frames.due() else {
            panic!("a draft is due");
        };
        let config = block::Config::new(1 << 16).expect("the budget fits");
        let memory = block::Heap::new(config.reservation());
        let pool = block::Pool::new(config, memory);
        let draft = layout
            .draft(&pool, types::frame::Form::Encoded)
            .expect("a block");
        assert!(matches!(frames.draft(draft), hub::Step::Frame(_)));
        frames.lost()
    }

    #[test]
    fn opens_a_latest_session_for_a_latest_read() {
        assert_eq!(lost_before_a_frame(Mode::Latest), 0..0);
        assert_eq!(lost_before_a_frame(Mode::Complete), 5..9);
    }
}

#[test]
#[should_panic(expected = "an open of 65537 bytes is over the cap of 65536")]
fn panics_on_a_read_over_the_cap() {
    read(OPEN_BYTES_MAX + 1).encode(&mut [0; Read::LEN]);
}

#[test]
#[should_panic(expected = "an open of 65537 bytes is over the cap of 65536")]
fn panics_on_the_body_of_a_read_over_the_cap() {
    let _body = read(OPEN_BYTES_MAX + 1).body();
}

/// Each sample type at its largest counts. The matches fail to compile when a scalar
/// or a shape is added, so that a new one cannot pass the 304-byte bound unchecked.
fn largest_types() -> Vec<Type> {
    use Scalar::{
        Bool, F32, F64, I8, I16, I32, I64, Span, Stamp, U8, U16, U32, U64, Uuid,
    };
    let scalars = [
        Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Stamp, Span, Uuid,
    ];
    for scalar in scalars {
        match scalar {
            Bool | I8 | I16 | I32 | I64 | U8 | U16 | U32 | U64 | F32 | F64 | Stamp
            | Span | Uuid => {}
        }
    }
    let sides = Sides {
        rows: u16::MAX,
        columns: u16::MAX,
    };
    let mut types = vec![Type::String, Type::Bytes];
    for element in scalars {
        types.extend([
            Type::Scalar(element),
            Type::Array {
                element,
                len: u32::MAX,
            },
            Type::List {
                element,
                max: u32::MAX,
            },
            Type::Matrix { element, sides },
        ]);
    }
    for sample in &types {
        match sample {
            Type::Scalar(_)
            | Type::Array { .. }
            | Type::List { .. }
            | Type::String
            | Type::Bytes
            | Type::Matrix { .. } => {}
        }
    }
    types
}

#[test]
fn fits_the_largest_place_in_304_bytes() {
    let name: Name = "a".repeat(Name::MAX_BYTES).parse().unwrap();
    let places: Vec<_> = largest_types()
        .into_iter()
        .map(|data_type| Place {
            key: channel::Key::from_u128(1),
            data_type,
            name: name.clone(),
            index: usize::try_from(u32::MAX).unwrap(),
        })
        .collect();
    for place in &places {
        let one = std::slice::from_ref(place);
        let runs: Vec<_> = place::messages(one, 304).map(decode_message).collect();
        assert_eq!(runs, [one], "{}", place.data_type);
    }
    assert_eq!(places.iter().map(place_len).max(), Some(304));
}

#[test]
#[should_panic(expected = "out has 26 bytes, and the message has 25")]
fn panics_on_a_message_into_out_of_another_length() {
    let places = [pt(1, "u8", "a")];
    let [message] = place::messages(&places, 25).collect::<Vec<_>>()[..] else {
        panic!("one place is one message");
    };
    message.encode(&mut [0; 26]);
}

#[test]
#[should_panic(expected = "a place of 25 bytes is over a message of 24")]
fn panics_on_a_place_over_a_message() {
    place::messages(&[pt(1, "u8", "a")], 24).for_each(drop);
}

fn selector() -> impl Strategy<Value = Selector> {
    let segment = prop_oneof![Just("*".to_owned()), "[a-z0-9_]{1,6}"];
    let pattern = (any::<bool>(), proptest::collection::vec(segment, 1..4)).prop_map(
        |(excluded, segments)| {
            let text = segments.join(".");
            if excluded { format!("!{text}") } else { text }
        },
    );
    proptest::collection::vec(pattern, 1..6)
        .prop_filter_map("the selector does not read", |patterns| {
            Selector::new(patterns.iter().map(String::as_str)).ok()
        })
}

fn sample() -> impl Strategy<Value = Type> {
    let element = prop_oneof![
        Just(Scalar::Bool),
        Just(Scalar::U8),
        Just(Scalar::F64),
        Just(Scalar::Stamp),
        Just(Scalar::Span),
        Just(Scalar::Uuid),
    ];
    (element, any::<u32>(), any::<u16>(), 0..6_u8).prop_map(
        |(element, n, side, form)| match form {
            0 => Type::Scalar(element),
            1 => Type::Array { element, len: n },
            2 => Type::List { element, max: n },
            3 => Type::String,
            4 => Type::Bytes,
            _ => Type::Matrix {
                element,
                sides: Sides {
                    rows: side,
                    columns: side,
                },
            },
        },
    )
}

fn places() -> impl Strategy<Value = Vec<Place>> {
    let place = (
        any::<u128>(),
        sample(),
        "[a-z_]{1,8}(\\.[a-z0-9_]{1,8}){0,3}",
    )
        .prop_map(|(key, data_type, name)| Place {
            key: channel::Key::from_u128(key),
            data_type,
            name: name.parse().unwrap(),
            index: 0,
        });
    proptest::collection::vec(place, 1..8)
}

proptest! {
    #[test]
    fn round_trips_an_open(select in selector(), complete in any::<bool>()) {
        let mode = if complete { Mode::Complete } else { Mode::Latest };
        let open = Open { mode, select };
        prop_assert_eq!(Open::decode(&encode_open(&open)), Ok(open));
    }

    #[test]
    fn round_trips_a_read(
        length in 0..=OPEN_BYTES_MAX,
        signature in any::<[u8; 32]>(),
    ) {
        let mut read = read(length);
        read.signature[32..].copy_from_slice(&signature);
        prop_assert_eq!(Read::decode(&encode_read(read)), Ok(read));
        prop_assert_eq!(First::decode(&encode_read(read)), Ok(First::Read(read)));
    }

    #[test]
    fn round_trips_places(count in 1..=PLACES_MAX) {
        let places = Places { count };
        prop_assert_eq!(Places::decode(&encode_places(places)), Ok(places));
    }

    #[test]
    fn refuses_each_count_over_the_cap(count in PLACES_MAX + 1..=u32::MAX) {
        let mut bytes = vec![6];
        bytes.extend(count.to_le_bytes());
        prop_assert_eq!(
            Places::decode(&bytes),
            Err(Error::Places { count, cap: PLACES_MAX })
        );
    }

    /// The reader gives with the last place, and at no message before it, the start:
    /// each place of the run, and the decoder of the replies. When the index place is
    /// not a stamp, the last message gives `Unstamped`.
    #[test]
    fn decodes_a_run_cut_into_messages_of_whole_places(
        (mut places, index) in places().prop_flat_map(|places| {
            let len = places.len();
            (Just(places), 0..len)
        }),
        max in 100..1_000_usize,
        complete in any::<bool>(),
        stamp in any::<bool>(),
    ) {
        for place in &mut places {
            place.index = index;
        }
        if stamp {
            places[index].data_type = "timestamp".parse().unwrap();
        }
        let index_type = places[index].data_type;
        let stamped = index_type == "timestamp".parse().unwrap();
        let mode = if complete { Mode::Complete } else { Mode::Latest };
        let mut reader = Reader::new(mode);
        let count = u32::try_from(places.len()).unwrap();
        let first = encode_places(Places { count });
        prop_assert!(matches!(reader.decode(Some(&first)), Ok(None)));
        let messages: Vec<_> = place::messages(&places, max).collect();
        let mut last = None;
        for (at, message) in messages.iter().enumerate() {
            let end = at.saturating_add(1) == messages.len();
            match reader.decode(Some(&encode_message(*message))) {
                Ok(start) => {
                    prop_assert_eq!(start.is_some(), end);
                    last = start;
                }
                Err(error) if end && !stamped => {
                    let unstamped = Error::Unstamped {
                        place: u32::try_from(index).unwrap(),
                        sample: index_type,
                    };
                    prop_assert_eq!(error, unstamped);
                    return Ok(());
                }
                Err(error) => prop_assert!(false, "{error:?}"),
            }
        }
        prop_assert!(stamped);
        let start = last.expect("the last place gives the start");
        prop_assert_eq!(&start.places, &places);
        prop_assert_eq!(start.set.groups(), [index]);
        let mut home = start.home;
        let opened = crate::hub::Reply::Opened { live: 7 };
        let opened = crate::hub::tests::encode_reply(opened);
        let opened = home.decode(Some(&opened));
        let live = match opened {
            Ok(crate::hub::FromHome::Opened { live }) => Some(live),
            _ => None,
        };
        prop_assert_eq!(live, Some(7));
    }

    /// A fault after any message of a cut run but the last gives its exact error: a
    /// second `Places` or a reply reads as run bytes too short for a place, and a
    /// message of one more place than remain gives `Run`.
    #[test]
    fn refuses_a_fault_after_any_message_of_a_run(
        mut places in places(),
        max in 100..1_000_usize,
        at in any::<prop::sample::Index>(),
        fault in 0..3_u8,
    ) {
        places[0].data_type = "timestamp".parse().unwrap();
        let count = u32::try_from(places.len()).unwrap();
        let reply = Places { count };
        let mut reader = Reader::new(Mode::Complete);
        reader.decode(Some(&encode_places(reply)))?;
        let messages: Vec<_> = place::messages(&places, max).collect();
        let at = at.index(messages.len());
        let mut sent = 0_usize;
        for message in messages.into_iter().take(at) {
            let bytes = encode_message(message);
            prop_assert!(matches!(reader.decode(Some(&bytes)), Ok(None)));
            sent = sent.saturating_add(place::decode(&bytes)?.len());
        }
        let (bytes, error) = match fault {
            0 => (encode_places(reply), Error::Length { len: Places::LEN }),
            1 => (vec![1], Error::Length { len: 1 }),
            _ => {
                let mut over = places[sent..].to_vec();
                over.push(places[0].clone());
                let remain = count.saturating_sub(u32::try_from(sent).unwrap());
                (encode_run(&over), Error::Run { items: over.len(), remain })
            }
        };
        prop_assert_eq!(reader.decode(Some(&bytes)).err(), Some(error));
    }

    #[test]
    fn cuts_a_run_into_full_messages_under_the_max(
        places in places(),
        max in 100..300_usize,
    ) {
        let messages: Vec<_> = place::messages(&places, max)
            .map(|message| (message.encoded_len(), decode_message(message)))
            .collect();
        for (len, decoded) in &messages {
            prop_assert!(*len <= max);
            prop_assert_eq!(*len, decoded.iter().map(place_len).sum::<usize>());
        }
        for ((len, _), (_, next)) in messages.iter().zip(messages.iter().skip(1)) {
            prop_assert!(len.saturating_add(place_len(&next[0])) > max);
        }
        let cut: Vec<_> =
            messages.into_iter().flat_map(|(_, decoded)| decoded).collect();
        prop_assert_eq!(cut, places);
    }
}

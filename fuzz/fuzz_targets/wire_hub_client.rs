//! The decoders of `wire::hub::client` and `wire::hub::write::Reply::decode` never
//! panic, each but the two opens, `Reader::decode`, and `Opening::decode` refuses a
//! message of another kind with its `Kind` error, `Reader::decode` refuses a first
//! message of another kind with it, each message that decodes encodes to its bytes, a
//! `Places` of 0 or over `PLACES_MAX` gives `Error::Places`, each body ends at its
//! length and nowhere else, each refusal of a body is the one that its rest gives, a
//! read stream gives with its last place the places of its run, their key set, and the
//! decoder of a session of their count and the mode, a read of more than one index
//! gives `Error::ManyIndexes` after `Error::Repeated` and ends its decoder, each other
//! refused message of a read stream leaves its decoder as it was, an `Opening` gives
//! what a `Reader` gives other than `Error::ManyIndexes`, and each valid message made
//! from the input reads back. A `Reader` holds an `Opening`,
//! so the checks of the read stream are the checks of the run of both.
//! `write::Replies` takes or refuses each reply as the order of the replies gives, and
//! a refused reply changes no count.
//!
//! Input: the messages of one stream (`fuzz::messages`). Each decoder reads each
//! message, an open among them; the body of the first message, when it is a request, a
//! response, a read, or a write, takes each later message; and a `client::Reader` of
//! each mode and a `write::Opening` take each message until they give the last place,
//! and then the end of the stream. A `write::Replies` counts a frame sent for each
//! message whose first byte is 0, with a group for each other byte of it, or group 0
//! when it has none, and decodes each other message as a reply.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::authority::Authority;
use types::channel;
use types::connection;
use types::ed25519::PublicKey;
use types::frame;
use types::hello::Hello;
use types::name::Selector;
use types::node;
use types::sample::{Scalar, Type};
use types::time::{Interval, Span, Stamp};
use wire::hub;
use wire::hub::client::write::{self, Opening};
use wire::hub::client::{
    BODY_BYTES_MAX, Body, Challenge, Error, First, Mode, OPEN_BYTES_MAX, Open,
    PLACES_MAX, Place, Places, Read, Reader, Request, Response, Signed, place,
};
use wire::hub::write::{End, Outcome, Refusal, Replies, Reply};

/// Checks one decoder of kind `kind` on `message`: a decoded message encodes to it,
/// and a refusal is the one that its first byte gives, or one that `allowed` takes.
fn decode<T>(
    message: &[u8],
    kind: u8,
    decode: fn(&[u8]) -> Result<T, Error>,
    encode: fn(&T) -> Vec<u8>,
    allowed: fn(&Error) -> bool,
) -> Option<T> {
    match (message.first(), decode(message)) {
        (_, Ok(decoded)) => {
            assert_eq!(encode(&decoded), message, "the message changed");
            Some(decoded)
        }
        (None, Err(error)) => {
            panic!("{error:?} is not the refusal of an empty message")
        }
        (Some(&first), Err(error)) if first != kind => {
            assert_eq!(error, Error::Kind { kind: first }, "{message:?}");
            None
        }
        (Some(_), Err(error)) => {
            assert!(
                allowed(&error),
                "{error:?} is not a refusal of the bytes of {message:?}"
            );
            None
        }
    }
}

fn challenge(challenge: &Challenge) -> Vec<u8> {
    let mut out = vec![0; Challenge::LEN];
    challenge.encode(&mut out);
    out
}

fn signed(signed: &Signed) -> Vec<u8> {
    let mut out = vec![0; signed.encoded_len()];
    signed.encode(&mut out);
    out
}

fn request(request: &Request) -> Vec<u8> {
    let mut out = vec![0; Request::LEN];
    request.encode(&mut out);
    out
}

fn response(response: &Response) -> Vec<u8> {
    let mut out = vec![0; Response::LEN];
    response.encode(&mut out);
    out
}

fn read_message(read: &Read) -> Vec<u8> {
    let mut out = vec![0; Read::LEN];
    read.encode(&mut out);
    out
}

fn write_message(write: &write::Write) -> Vec<u8> {
    let mut out = vec![0; write::Write::LEN];
    write.encode(&mut out);
    out
}

fn reply(reply: &Reply) -> Vec<u8> {
    let mut out = vec![0; reply.encoded_len()];
    reply.encode(&mut out);
    out
}

fn places(places: &Places) -> Vec<u8> {
    let mut out = vec![0; Places::LEN];
    places.encode(&mut out);
    out
}

/// `run` as one message of a run of places.
fn encode_run(run: &[Place]) -> Vec<u8> {
    let mut messages = place::messages(run, usize::MAX);
    let message = messages.next().expect("a run of places is a message");
    assert!(messages.next().is_none(), "a run of places is one message");
    let mut out = vec![0; message.encoded_len()];
    message.encode(&mut out);
    out
}

/// Checks the decoders of a message with no kind byte, the opens, and of a reply,
/// whose first byte is one of five kinds: one that decodes must encode to its bytes.
fn unkinded(message: &[u8]) {
    if let Ok(open) = Open::decode(message) {
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(out, message, "the open changed");
    }
    if let Ok(open) = write::Open::decode(message) {
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(out, message, "the open of a write changed");
    }
    match (message.first(), Reply::decode(message)) {
        (_, Ok(decoded)) => assert_eq!(reply(&decoded), message, "the reply changed"),
        (None, Err(error)) => assert_eq!(error, hub::Error::Empty),
        (Some(&kind), Err(error)) if !(1..=5).contains(&kind) => {
            assert_eq!(error, hub::Error::Kind { kind }, "{message:?}");
        }
        (Some(_), Err(error)) => assert!(
            matches!(
                error,
                hub::Error::Length { .. }
                    | hub::Error::Count { .. }
                    | hub::Error::Range { .. }
                    | hub::Error::Refusal { .. }
                    | hub::Error::End { .. }
            ),
            "{error:?} is not a refusal of the bytes of {message:?}"
        ),
    }
}

/// Drives an `Opening` and a `Reader` on `bytes`: each message gives what it gives
/// the reader, and the start gives the reader's places and key set. Only the reader
/// refuses places of more than one index, after the start.
fn open(bytes: &[u8]) {
    let mut opening = Opening::default();
    let mut reader = Reader::new(Mode::Latest);
    for message in fuzz::messages(bytes) {
        let opened = opening.decode(Some(message));
        let read = reader.decode(Some(message));
        match (opened, read) {
            (Ok(None), Ok(None)) => {}
            (Ok(Some(opened)), Ok(Some(read))) => {
                assert_eq!(opened.places, read.places, "the places differ");
                assert_eq!(
                    format!("{:?}", opened.set),
                    format!("{:?}", read.set),
                    "the set differs"
                );
                return;
            }
            (Ok(Some(opened)), Err(read @ Error::ManyIndexes { .. })) => {
                assert!(opened.set.groups().len() > 1, "{read:?} of one index");
                assert_eq!(many(&opened.places), Some(read), "{message:?}");
                return;
            }
            (Err(opened), Err(read)) => assert_eq!(opened, read, "{message:?}"),
            (opened, read) => {
                panic!("{opened:?} is not what the reader gave: {read:?}")
            }
        }
    }
    assert_eq!(opening.decode(None).err(), Some(Error::Finished));
}

/// Drives a reader of `mode` on `bytes` until it gives the last place, then ends the
/// stream. `Error::ManyIndexes` ends the reader, each other refused message leaves it
/// as it was, and a refused first message
/// gives the error of its places. Each message that it takes decodes alone as places or
/// as a run that encodes to it, a run holds no more places than remain, and only the
/// message with the last place gives the start: the places of the run, their key set in
/// place order with the index of each place, and the decoder of a session of the count
/// and the mode. An end before the last place gives `Error::Finished`.
fn drive(bytes: &[u8], mode: Mode) {
    let mut reader = Reader::new(mode);
    let mut run: Option<(Places, u32, Vec<Place>)> = None;
    for message in fuzz::messages(bytes) {
        let start = match reader.decode(Some(message)) {
            Ok(start) => start,
            Err(error) => {
                let done = matches!(error, Error::ManyIndexes { .. });
                if run.is_none() {
                    let places = Places::decode(message).err();
                    assert_eq!(
                        places.as_ref(),
                        Some(&error),
                        "not the error of the places"
                    );
                }
                if let Some(expected) = refusal(run.as_ref(), message) {
                    assert_eq!(Some(error), expected, "a valid message refused");
                }
                if done {
                    return;
                }
                continue;
            }
        };
        let Some((reply, remain, all)) = &mut run else {
            let places = Places::decode(message).expect("the places decode alone");
            let mut out = [0; Places::LEN];
            places.encode(&mut out);
            assert_eq!(out, message, "the places changed");
            assert!(start.is_none(), "the places gave the start");
            run = Some((places, places.count, Vec::new()));
            continue;
        };
        let places = place::decode(message).expect("the run decodes alone");
        assert_eq!(encode_run(&places), message, "the run changed");
        *remain -= u32::try_from(places.len()).expect("the run fits a u32");
        all.extend(places);
        let Some(start) = start else {
            assert_ne!(*remain, 0, "the last place gave no start");
            continue;
        };
        assert_eq!(*remain, 0, "a start before the last place");
        assert_eq!(start.places, *all, "the places differ");
        let keys = start.set.entries().iter().map(|entry| entry.key);
        assert!(
            keys.eq(all.iter().map(|place| place.key)),
            "the set differs"
        );
        let groups: Vec<_> = (all.iter().enumerate())
            .filter(|(at, place)| place.index == *at)
            .map(|(at, _)| at)
            .collect();
        assert_eq!(start.set.groups(), groups, "the groups differ");
        for (at, place) in all.iter().enumerate() {
            assert_eq!(start.set.index(at), place.index, "the index differs");
        }
        let session = hub::Reader::new(&hub::Open {
            mode: match mode {
                Mode::Latest => hub::Mode::Latest,
                Mode::Complete => hub::Mode::Complete { limit_bytes: 0 },
            },
            channels: reply.count,
        });
        // `hub::Reader` has no `PartialEq`. Its derived `Debug` shows each field, so
        // an equal text is an equal state; `wire_hub_reader` checks its replies.
        assert_eq!(
            format!("{:?}", start.home),
            format!("{session:?}"),
            "the home differs"
        );
        return;
    }
    assert_eq!(reader.decode(None).err(), Some(Error::Finished));
}

/// The error of the first of `places` whose index is not a place whose index is
/// itself, or that is such an index and not a stamp.
fn unindexed(places: &[Place]) -> Option<Error> {
    let fit = "the places fit a u32";
    places.iter().enumerate().find_map(|(at, place)| {
        let index = places
            .get(place.index)
            .filter(|own| own.index == place.index);
        let at_place = u32::try_from(at).expect(fit);
        match index {
            None => Some(Error::Index {
                place: at_place,
                index: u32::try_from(place.index).expect(fit),
            }),
            Some(own)
                if at == place.index
                    && own.data_type != Type::Scalar(Scalar::Stamp) =>
            {
                Some(Error::Unstamped {
                    place: at_place,
                    sample: own.data_type,
                })
            }
            Some(_) => None,
        }
    })
}

/// `Error::ManyIndexes` for the first of `places` whose index is not that of place 0,
/// as a read has one index.
fn many(places: &[Place]) -> Option<Error> {
    let fit = "the places fit a u32";
    let lead = places.first()?.index;
    let (at, place) =
        (places.iter().enumerate()).find(|(_, place)| place.index != lead)?;
    Some(Error::ManyIndexes {
        place: u32::try_from(at).expect(fit),
        index: u32::try_from(place.index).expect(fit),
    })
}

/// `None` when `message` is not a valid message of `run`. Else, when it holds the last
/// place, `Some` of the error of the indexes of its places and those before them, of
/// the least key that they name twice, or of a second index, and else `Some(None)`.
fn refusal(
    run: Option<&(Places, u32, Vec<Place>)>,
    message: &[u8],
) -> Option<Option<Error>> {
    let (_, remain, all) = run?;
    let places = place::decode(message).ok()?;
    let remain_len = usize::try_from(*remain).ok()?;
    if places.len() > remain_len {
        return None;
    }
    if places.len() < remain_len {
        return Some(None);
    }
    let all: Vec<_> = all.iter().chain(&places).cloned().collect();
    if let Some(error) = unindexed(&all) {
        return Some(Some(error));
    }
    let mut keys: Vec<_> = all.iter().map(|place| place.key).collect();
    keys.sort_unstable();
    if let Some(pair) = keys.windows(2).find(|pair| pair[0] == pair[1]) {
        return Some(Some(Error::Repeated { key: pair[0] }));
    }
    Some(many(&all))
}

/// Checks each decoder on `message`, and gives its body when it is a request, a
/// response, or a read.
fn fixed(message: &[u8]) -> Option<(Body, u64)> {
    unkinded(message);
    let first = match *message {
        [5, ..] => Request::decode(message).map(First::Request),
        [6, ..] => Read::decode(message).map(First::Read),
        [7, ..] => write::Write::decode(message).map(First::Write),
        [kind, ..] => Err(Error::Kind { kind }),
        [] => Err(Error::Empty),
    };
    assert_eq!(First::decode(message), first, "{message:?}");
    if message.is_empty() {
        assert_eq!(Challenge::decode(message), Err(Error::Empty));
        assert_eq!(Signed::decode(message), Err(Error::Empty));
        assert_eq!(Request::decode(message), Err(Error::Empty));
        assert_eq!(Response::decode(message), Err(Error::Empty));
        assert_eq!(Read::decode(message), Err(Error::Empty));
        assert_eq!(write::Write::decode(message), Err(Error::Empty));
        assert_eq!(Places::decode(message), Err(Error::Empty));
        return None;
    }
    decode(message, 4, Challenge::decode, challenge, length);
    decode(message, 4, Signed::decode, signed, |error| {
        matches!(
            error,
            Error::Length { .. } | Error::Subject(_) | Error::SmallOrder
        )
    });
    decode(message, 6, Places::decode, places, |error| {
        matches!(error, Error::Length { .. } | Error::Places { .. })
    });
    if let [6, a, b, c, d] = *message {
        let count = u32::from_le_bytes([a, b, c, d]);
        if !(1..=PLACES_MAX).contains(&count) {
            let over = Err(Error::Places {
                count,
                cap: PLACES_MAX,
            });
            assert_eq!(Places::decode(message), over, "{message:?}");
        }
    }
    let sent = decode(message, 5, Request::decode, request, oversize);
    let answered = decode(message, 5, Response::decode, response, oversize);
    let opened = decode(message, 6, Read::decode, read_message, oversize);
    let wrote = decode(message, 7, write::Write::decode, write_message, oversize);
    let (body, length, cap) = match (sent, answered, opened, wrote) {
        (Some(sent), None, None, None) => (sent.body(), sent.length, BODY_BYTES_MAX),
        (None, Some(answered), None, None) => {
            (answered.body(), answered.length, BODY_BYTES_MAX)
        }
        (None, None, Some(opened), None) => {
            (opened.body(), opened.length, OPEN_BYTES_MAX)
        }
        (None, None, None, Some(wrote)) => (wrote.body(), wrote.length, OPEN_BYTES_MAX),
        (None, None, None, None) => return None,
        _ => panic!("{message:?} is more than one first message"),
    };
    assert!(length <= cap, "a body over the cap came");
    Some((body, length))
}

/// A refusal of a message of a fixed length.
fn length(error: &Error) -> bool {
    matches!(error, Error::Length { .. })
}

/// A refusal of a first message with a body: its length, or its body over the cap.
fn oversize(error: &Error) -> bool {
    matches!(error, Error::Length { .. } | Error::Oversize { .. })
}

/// Each decoder must read each message, and each message after the first must be
/// taken or refused as the rest of the body of the first gives.
fn read(bytes: &[u8]) {
    drive(bytes, Mode::Latest);
    drive(bytes, Mode::Complete);
    open(bytes);
    let mut messages = fuzz::messages(bytes);
    let Some((mut body, length)) = messages.next().and_then(fixed) else {
        for message in messages {
            fixed(message);
        }
        return;
    };
    let mut remain = usize::try_from(length).expect("the body fits");
    assert_eq!(body.remain(), remain, "the body has another length");
    for message in messages {
        fixed(message);
        let len = message.len();
        let expected = if remain == 0 {
            Err(Error::Trailing)
        } else if len == 0 {
            Err(Error::Empty)
        } else if len > remain {
            Err(Error::Body { len, remain })
        } else {
            remain -= len;
            Ok(message)
        };
        assert_eq!(body.take(message), expected, "{message:?}");
        assert_eq!(body.remain(), remain, "the rest of the body changed");
    }
    let ended = match remain {
        0 => Ok(()),
        remain => Err(Error::Unfinished { remain }),
    };
    assert_eq!(body.end(), ended, "the body ended at another length");
}

/// Drives a `Replies` on `bytes`: a message whose first byte is 0, a kind that no reply
/// has, counts a frame sent, whose groups are its other bytes, or group 0 when it has
/// none. Each other message must be taken or refused as a model of the order of the
/// replies gives.
fn replies(bytes: &[u8]) {
    let mut replies = Replies::new();
    let mut groups: Vec<Vec<u32>> = Vec::new();
    // The frames whose groups each have an outcome, and the groups of the next frame
    // that have one.
    let (mut answered, mut at) = (0_u64, 0);
    let (mut taken, mut stored) = (0, 0);
    for message in fuzz::messages(bytes) {
        let sent = u64::try_from(groups.len()).expect("a u64 holds a usize");
        if let [0, rest @ ..] = message {
            let frame: Vec<u32> = match rest {
                [] => vec![0],
                rest => rest.iter().map(|&group| u32::from(group)).collect(),
            };
            replies.sent(frame.iter().copied());
            groups.push(frame);
        } else {
            let next =
                usize::try_from(answered).expect("a usize holds the frames sent");
            let due = groups.get(next).map_or(0, |groups| groups[at]);
            let want = match Reply::decode(message) {
                Ok(Reply::Outcome { frame, .. }) if frame == 0 || frame > sent => {
                    Err(hub::Error::Unasked { frame, sent })
                }
                Ok(Reply::Outcome { frame, .. }) if frame != answered + 1 => {
                    Err(hub::Error::Order {
                        frame,
                        next: answered + 1,
                    })
                }
                Ok(Reply::Outcome { frame, group, .. }) if group != due => {
                    Err(hub::Error::Group {
                        frame,
                        group,
                        next: due,
                    })
                }
                Ok(reply @ Reply::Outcome { frame, outcome, .. }) => {
                    at += 1;
                    if at == groups[next].len() {
                        answered += 1;
                        at = 0;
                    }
                    if let Outcome::Taken { .. } = outcome {
                        taken = frame;
                    }
                    Ok(reply)
                }
                Ok(Reply::Stored { frames }) if frames < stored => {
                    Err(hub::Error::Stored {
                        frames,
                        bound: stored,
                    })
                }
                Ok(Reply::Stored { frames }) if frames > answered => {
                    Err(hub::Error::Stored {
                        frames,
                        bound: answered,
                    })
                }
                Ok(reply @ Reply::Stored { frames }) => {
                    stored = frames;
                    Ok(reply)
                }
                Err(error) => Err(error),
            };
            assert_eq!(replies.decode(message), want, "{message:?}");
        }
        let sent = u64::try_from(groups.len()).expect("a u64 holds a usize");
        assert_eq!(replies.answered(), answered == sent, "the outcomes differ");
        let all = answered == sent && stored >= taken;
        assert_eq!(replies.stored(), all, "the stored frames differ");
    }
}

/// Each valid message made from `input` must decode to itself. A key of small order
/// or a subject that is not a name is not valid, so it becomes a fixed one.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let key = PublicKey::new(input.arbitrary()?)
        .unwrap_or_else(|_small| PublicKey::new([9; 32]).expect("a valid key"));
    let subject = input
        .arbitrary::<&str>()?
        .parse()
        .unwrap_or_else(|_bad| "ops.ana".parse().expect("a name"));
    let signed = Signed {
        hello: Hello {
            subject,
            key,
            via: node::Key::from_u128(input.arbitrary()?),
            connection: connection::Key(input.arbitrary()?),
            nonce: input.arbitrary()?,
            expires: Stamp::from_nanos(input.arbitrary()?),
        },
        signature: input.arbitrary()?,
    };
    let mut out = vec![0; signed.encoded_len()];
    signed.encode(&mut out);
    assert_eq!(Signed::decode(&out), Ok(signed), "the hello changed");

    let request = Request {
        length: input.int_in_range(0..=BODY_BYTES_MAX)?,
        signature: input.arbitrary()?,
    };
    let mut out = [0; Request::LEN];
    request.encode(&mut out);
    assert_eq!(Request::decode(&out), Ok(request), "the request changed");

    let challenge = Challenge {
        nonce: input.arbitrary()?,
        now: Interval {
            earliest: Stamp::from_nanos(input.arbitrary()?),
            latest: Stamp::from_nanos(input.arbitrary()?),
        },
    };
    let mut out = [0; Challenge::LEN];
    challenge.encode(&mut out);
    assert_eq!(
        Challenge::decode(&out),
        Ok(challenge),
        "the challenge changed"
    );

    let response = Response {
        length: input.int_in_range(0..=BODY_BYTES_MAX)?,
    };
    let mut out = [0; Response::LEN];
    response.encode(&mut out);
    assert_eq!(Response::decode(&out), Ok(response), "the response changed");
    write_read(input)
}

/// Each valid message of a read stream made from `input` must decode to itself.
fn write_read(input: &mut Unstructured) -> arbitrary::Result<()> {
    let read = Read {
        length: input.int_in_range(0..=OPEN_BYTES_MAX)?,
        signature: input.arbitrary()?,
    };
    let mut out = [0; Read::LEN];
    read.encode(&mut out);
    assert_eq!(
        First::decode(&out),
        Ok(First::Read(read)),
        "the read changed"
    );

    let reply = Places {
        count: input.int_in_range(1..=PLACES_MAX)?,
    };
    let mut out = [0; Places::LEN];
    reply.encode(&mut out);
    assert_eq!(Places::decode(&out), Ok(reply), "the places changed");

    let patterns: Vec<&str> = input.arbitrary()?;
    if let Ok(select) = Selector::new(patterns) {
        let mode = if input.arbitrary()? {
            Mode::Complete
        } else {
            Mode::Latest
        };
        let open = Open { mode, select };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(Open::decode(&out), Ok(open), "the open changed");
    }

    let data_type = input.arbitrary::<&str>()?.parse();
    let name = input.arbitrary::<&str>()?.parse();
    if let (Ok(data_type), Ok(name)) = (data_type, name) {
        let run = [Place {
            key: channel::Key::from_u128(input.arbitrary()?),
            data_type,
            name,
            index: usize::try_from(input.arbitrary::<u32>()?)
                .expect("a u32 fits a usize"),
        }];
        assert_eq!(
            place::decode(&encode_run(&run)),
            Ok(run.to_vec()),
            "the place changed"
        );
    }
    write_write(input)
}

/// Each valid message of a write stream made from `input` must decode to itself.
fn write_write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let wrote = write::Write {
        length: input.int_in_range(0..=OPEN_BYTES_MAX)?,
        signature: input.arbitrary()?,
    };
    assert_eq!(
        First::decode(&write_message(&wrote)),
        Ok(First::Write(wrote)),
        "the write changed"
    );

    let names: Vec<&str> = input.arbitrary()?;
    let channels: Vec<_> = names.iter().filter_map(|name| name.parse().ok()).collect();
    if !channels.is_empty() {
        let lease: Option<i64> = input.arbitrary()?;
        let open = write::Open {
            authority: Authority(input.arbitrary()?),
            lease: lease.map(Span::from_nanos),
            channels,
        };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(write::Open::decode(&out), Ok(open), "the open changed");
    }

    let count = input.int_in_range(1..=u32::MAX)?;
    let range = frame::Range {
        seq: input.int_in_range(0..=u64::MAX - u64::from(count))?,
        count,
    };
    let frame = input.arbitrary()?;
    let group = input.arbitrary()?;
    let outcome = |outcome| Reply::Outcome {
        frame,
        group,
        outcome,
    };
    let sent = match input.int_in_range(0..=12)? {
        0 => outcome(Outcome::Taken { range }),
        1 => outcome(Outcome::Lost { range }),
        2 => Reply::Stored { frames: range.seq },
        3 => outcome(Outcome::Refused(Refusal::Waiting)),
        4 => outcome(Outcome::Refused(Refusal::Reserved)),
        5 => outcome(Outcome::Refused(Refusal::Expired)),
        6 => outcome(Outcome::Refused(Refusal::Order)),
        7 => outcome(Outcome::Refused(Refusal::Codec)),
        8 => outcome(Outcome::Refused(Refusal::Full)),
        9 => outcome(Outcome::Ended(End::Removed)),
        10 => outcome(Outcome::Ended(End::Failed)),
        _ => outcome(Outcome::Refused(Refusal::Large)),
    };
    assert_eq!(Reply::decode(&reply(&sent)), Ok(sent), "the reply changed");
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    replies(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});

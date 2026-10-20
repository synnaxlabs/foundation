//! The hub messages on a frame's path, the frames of a warm reader session, the head,
//! the frames, and the replies of a program's write, the blob heads, and the body
//! counters of client requests and blob chunks, make no heap allocation, and the place
//! messages of a read make a known count. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::sync::Arc;

use types::{
    channel,
    frame::{
        self, Form, Path, Range,
        key_set::{Group, Interner, KeySet},
    },
    sample::{Scalar, Type},
};
use wire::hub::{
    Credit, Due, Frames, FromHome, FromReader, Head, Home, Mode, Open, Reader, Reply,
    Step, ends, keys,
    write::{self, Outcome, Refusal, Time},
};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const OPEN: Open = Open {
    mode: Mode::Latest,
    channels: 100_000,
};

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    head();
    credit();
    for series in [1, 1_000, 100_000_u32] {
        ends(series);
    }
    ends_by_place();
    frames();
    write_frames();
    write_head();
    write_replies();
    client_body();
    blob_body();
    blob_heads();
    place_messages();
}

/// An `Opened` at the open position 0.
const OPENED: [u8; 9] = [1, 0, 0, 0, 0, 0, 0, 0, 0];

/// A reader that has decoded `Opened` and a head of `series` series.
fn reader(series: u32) -> Reader {
    let mut out = [0; 18];
    Reply::Head(Head {
        path: Path::Live,
        range: Range { seq: 0, count: 1 },
        series,
    })
    .encode(&mut out);
    let mut reader = Reader::new(&OPEN);
    for message in [OPENED.as_slice(), &out] {
        reader.decode(Some(message)).expect("the head decodes");
    }
    reader
}

fn head() {
    let head = Head {
        path: Path::Live,
        range: Range { seq: 7, count: 1 },
        series: 3,
    };
    let mut out = [0; 18];
    let ((), allocations) = ALLOCATOR.count(|| Reply::Head(head).encode(&mut out));
    assert_eq!(allocations, 0, "the head encode allocated");
    let mut reader = Reader::new(&OPEN);
    reader.decode(Some(&OPENED)).expect("the session opens");
    let (decoded, allocations) = ALLOCATOR.count(|| match reader.decode(Some(&out)) {
        Ok(FromHome::Head(decoded)) => decoded,
        other => panic!("the head did not decode: {other:?}"),
    });
    assert_eq!(allocations, 0, "the head decode allocated");
    assert_eq!(decoded, head, "the head round trips");
}

fn write_head() {
    let head = write::Head {
        time: Time::Frame(Path::Backfill),
        form: Form::Encoded,
        count: 1_000,
        series: 3,
    };
    let mut out = [0; write::Head::LEN];
    let ((), allocations) = ALLOCATOR.count(|| head.encode(&mut out));
    assert_eq!(allocations, 0, "the write head encode allocated");
}

fn write_replies() {
    for reply in [
        write::Reply::Outcome {
            frame: 1,
            outcome: Outcome::Lost {
                range: Range { seq: 7, count: 3 },
            },
        },
        write::Reply::Outcome {
            frame: 2,
            outcome: Outcome::Refused(Refusal::Order),
        },
        write::Reply::Stored { frames: 9 },
    ] {
        let mut out = vec![0; reply.encoded_len()];
        let ((), allocations) = ALLOCATOR.count(|| reply.encode(&mut out));
        assert_eq!(allocations, 0, "the reply encode allocated: {reply:?}");
        let (decoded, allocations) = ALLOCATOR.count(|| write::Reply::decode(&out));
        assert_eq!(allocations, 0, "the reply decode allocated: {reply:?}");
        assert_eq!(decoded, Ok(reply), "the reply round trips");
    }
}

fn credit() {
    let credit = Credit { limit_bytes: 7 };
    let mut out = [0; Credit::LEN];
    let ((), allocations) = ALLOCATOR.count(|| credit.encode(&mut out));
    assert_eq!(allocations, 0, "the credit encode allocated");
    let mut open = [0; 13];
    Open {
        mode: Mode::Complete { limit_bytes: 0 },
        channels: 1,
    }
    .encode(&mut open);
    let mut key = [0; keys::LEN];
    keys::encode(&[channel::Key::from_u128(1)], &mut key);
    let mut home = Home::default();
    for message in [open.as_slice(), &key] {
        home.decode(message).expect("the open decodes");
    }
    let (decoded, allocations) = ALLOCATOR.count(|| match home.decode(&out) {
        Ok(FromReader::Credit(decoded)) => decoded,
        other => panic!("the credit did not decode: {other:?}"),
    });
    assert_eq!(allocations, 0, "the credit decode allocated");
    assert_eq!(decoded, credit, "the credit round trips");
}

fn ends(series: u32) {
    let mut run =
        vec![0; usize::try_from(series).expect("a u32 fits a usize") * ends::LEN];
    let ends = (0..series).map(|place| (place, place.wrapping_add(1)));
    let ((), allocations) = ALLOCATOR.count(|| ends::encode(ends, &mut run));
    assert_eq!(allocations, 0, "the encode of {series} ends allocated");
    let mut reader = reader(series);
    let (last, allocations) = ALLOCATOR.count(|| match reader.decode(Some(&run)) {
        Ok(FromHome::Ends { ends, last: true }) => ends.last(),
        other => panic!("the ends did not decode: {other:?}"),
    });
    assert_eq!(allocations, 0, "the decode of {series} ends allocated");
    assert_eq!(last, Some((series - 1, series)), "the last end round trips");
    let body = vec![7; usize::try_from(series).expect("a u32 fits a usize")];
    for (index, message) in body.chunks(body.len().div_ceil(2)).enumerate() {
        let ((at, last), allocations) = ALLOCATOR.count(|| {
            let at = reader.body();
            match reader.decode(Some(message)) {
                Ok(FromHome::Body { last, .. }) => (at, last),
                other => panic!("the body did not decode: {other:?}"),
            }
        });
        assert_eq!(allocations, 0, "the body of {series} ends allocated");
        assert_eq!(at, Some(index * body.len().div_ceil(2)), "the body moved");
        let end = at.map(|at| at + message.len());
        assert_eq!(last, end == Some(body.len()), "the body ends at {end:?}");
    }
}

/// Each input of a frame of a warm session, after the first frame. The draft comes
/// from the caller's pool outside the count.
fn frames() {
    let set = Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &[(channel::Key::from_u128(2), Type::Scalar(Scalar::I64))],
    }]);
    let config = block::Config::new(1 << 16).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    let pool = block::Pool::new(config, memory);
    let open = Open {
        mode: Mode::Latest,
        channels: 2,
    };
    let mut frames =
        Frames::open(Reader::new(&open), set, Some(&OPENED)).expect("opened");
    let mut run = [0; 2 * ends::LEN];
    ends::encode([(0, 16), (1, 32)], &mut run);
    let body = [7; 32];
    for round in 0..2 {
        let mut head = [0; 18];
        Reply::Head(Head {
            path: Path::Live,
            range: Range {
                seq: 2 * round,
                count: 2,
            },
            series: 2,
        })
        .encode(&mut head);
        let (steps, allocations) = ALLOCATOR.count(|| {
            let head = frames.decode(Some(&head));
            let ends = frames.decode(Some(&run));
            (head, ends, matches!(frames.due(), Ok(Due::Draft(_))))
        });
        assert!(
            matches!(steps, (Ok(Step::Head(_)), Ok(Step::More), true)),
            "the head and ends did not decode: {steps:?}"
        );
        if round == 1 {
            assert_eq!(allocations, 0, "the head and ends of a frame allocated");
        }
        let Ok(Due::Draft(layout)) = frames.due() else {
            panic!("a draft is due");
        };
        let draft = layout.draft(&pool, Form::Encoded).expect("a block");
        let (step, allocations) = ALLOCATOR.count(|| {
            let step = frames.draft(draft);
            let Ok(Due::Body(rest)) = frames.due() else {
                panic!("a body is due");
            };
            rest[..20].copy_from_slice(&body[..20]);
            (step, frames.received(20), frames.decode(Some(&body[20..])))
        });
        assert_eq!(allocations, 0, "the draft and body of a frame allocated");
        let (Step::More, Ok(Step::More), Ok(Step::Frame(frame))) = step else {
            panic!("the body did not give a frame: {step:?}");
        };
        assert_eq!(frame.series(1), Some(&body[16..]), "the body round trips");
    }
}

/// Each input of each frame of a program's write at the node, the first frame included.
/// The draft comes from the caller's pool outside the count.
fn write_frames() {
    let set = KeySet::ordered([
        (channel::Key::from_u128(1), Type::Scalar(Scalar::Stamp), 0),
        (channel::Key::from_u128(2), Type::Scalar(Scalar::I64), 0),
    ])
    .expect("no key repeats");
    let config = block::Config::new(1 << 16).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    let pool = block::Pool::new(config, memory);
    let mut frames = write::Frames::new(Arc::new(set));
    let mut head = [0; write::Head::LEN];
    write::Head {
        time: Time::Frame(Path::Live),
        form: Form::Raw,
        count: 2,
        series: 2,
    }
    .encode(&mut head);
    let mut run = [0; 2 * ends::LEN];
    ends::encode([(0, 16), (1, 32)], &mut run);
    let body = [7; 32];
    for round in 0..2 {
        let (steps, allocations) = ALLOCATOR.count(|| {
            let head = frames.decode(&head);
            let ends = frames.decode(&run);
            let due = matches!(frames.due(), Ok(write::Due::Draft { .. }));
            (head, ends, due)
        });
        assert!(
            matches!(
                steps,
                (Ok(write::Step::Head(_)), Ok(write::Step::More), true)
            ),
            "the head and ends did not decode: {steps:?}"
        );
        assert_eq!(
            allocations, 0,
            "the head and ends of write frame {round} allocated"
        );
        let Ok(write::Due::Draft { layout, form }) = frames.due() else {
            panic!("a draft is due");
        };
        let draft = layout.draft(&pool, form).expect("a block");
        let (step, allocations) = ALLOCATOR.count(|| {
            let step = frames.draft(draft);
            let Ok(write::Due::Body(rest)) = frames.due() else {
                panic!("a body is due");
            };
            rest[..20].copy_from_slice(&body[..20]);
            (step, frames.received(20), frames.decode(&body[20..]))
        });
        assert_eq!(allocations, 0, "the draft and body of a write allocated");
        let (
            write::Step::More,
            Ok(write::Step::More),
            Ok(write::Step::Frame { draft, .. }),
        ) = step
        else {
            panic!("the body did not give a frame: {step:?}");
        };
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.series(1), Some(&body[16..]), "the body round trips");
    }
}

/// A run that the home writes by place, split into messages of 184 ends.
fn ends_by_place() {
    let lens: Vec<_> = (0..400_u32).map(|place| (place, 3)).collect();
    let mut run = vec![0; lens.len() * ends::LEN];
    let ((), allocations) = ALLOCATOR.count(|| {
        let mut each = frame::ends(lens.iter().copied())
            .map(|(place, end)| (place, u32::try_from(end).expect("fits")));
        for message in run.chunks_mut(184 * ends::LEN) {
            ends::encode(each.by_ref(), message);
        }
    });
    assert_eq!(
        allocations, 0,
        "the encode of a run split by place allocated"
    );
}

/// The take of each message of a client request body.
fn client_body() {
    let body = [7; 1_000];
    let request = wire::hub::client::Request {
        length: 1_000,
        signature: [0; 64],
    };
    let mut rest = request.body();
    let (taken, allocations) = ALLOCATOR.count(|| {
        body.chunks(300)
            .map(|message| rest.take(message).expect("a body message").len())
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the take of a client body allocated");
    assert_eq!(taken, body.len(), "the whole body was taken");
}

/// The take of each message of a blob chunk's body.
fn blob_body() {
    let body = [7; 1_000];
    let put = wire::blob::Put {
        digest: types::digest::Digest([0; 32]),
        len: 1_000,
    };
    let mut rest = put.body();
    let (taken, allocations) = ALLOCATOR.count(|| {
        body.chunks(300)
            .map(|message| rest.take(message).expect("a body message").len())
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the take of a blob body allocated");
    assert_eq!(taken, body.len(), "the whole body was taken");
}

/// The decode of a put and a get at the server, and of each reply at the requester.
fn blob_heads() {
    use types::digest::Digest;
    use wire::blob::{FromRequester, Put, Reply, Requester, Server, get};

    let digest = Digest([7; 32]);
    let mut put = [0; Put::LEN];
    Put { digest, len: 64 }.encode(&mut put);
    let mut get = vec![0; get::encoded_len(2)];
    get::encode(&[digest, digest], &mut get);
    let server = Server::new(1 << 20);
    let (decoded, allocations) = ALLOCATOR.count(|| {
        [put.as_slice(), &get].map(|message| match server.decode(message) {
            Ok(FromRequester::Put(put)) => put.digest == digest,
            Ok(FromRequester::Get(mut digests)) => digests.all(|got| got == digest),
            other => panic!("a put or a get: {other:?}"),
        })
    });
    assert_eq!(allocations, 0, "the decode of a blob request allocated");
    assert_eq!(decoded, [true, true], "the put and the get decoded");

    let replies = [
        Reply::Chunk { digest, len: 64 },
        Reply::Absent { digest },
        Reply::Stored { digest },
    ]
    .map(|reply| {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        out
    });
    let requester = Requester::new(1 << 20);
    let (decoded, allocations) = ALLOCATOR.count(|| {
        replies
            .each_ref()
            .map(|message| match requester.decode(message) {
                Ok(Reply::Chunk { digest: got, len }) => got == digest && len == 64,
                Ok(Reply::Absent { digest: got } | Reply::Stored { digest: got }) => {
                    got == digest
                }
                other => panic!("a reply: {other:?}"),
            })
    });
    assert_eq!(allocations, 0, "the decode of a blob reply allocated");
    assert_eq!(decoded, [true, true, true], "each reply decoded");
}

/// The cut and the encode of a run of 3 places, one place a message. Each place is
/// formatted once at each cut that tries it and once at its encode: 8 times in all.
fn place_messages() {
    use wire::hub::client::{Mode, Place, Places, Reader, place};

    let places =
        [(1, "timestamp"), (2, "u8"), (3, "u8")].map(|(key, data_type)| Place {
            key: channel::Key::from_u128(key),
            data_type: data_type.parse().expect("a sample type"),
            name: "a".parse().expect("a name"),
            index: 0,
        });
    let mut out = [0; 82];
    let mut lens = [0; 3];
    let ((), allocations) = ALLOCATOR.count(|| {
        let mut rest = &mut out[..];
        for (message, len) in place::messages(&places, 32).zip(&mut lens) {
            *len = message.encoded_len();
            let (into, after) = rest.split_at_mut(*len);
            message.encode(into);
            rest = after;
        }
    });
    assert_eq!(allocations, 8, "the place messages allocated another count");
    assert_eq!(lens, [32, 25, 25], "each place is a message");
    let mut reader = Reader::new(Mode::Complete);
    let mut first = [0; Places::LEN];
    Places { count: 3 }.encode(&mut first);
    reader.decode(Some(&first)).expect("the places decode");
    let mut rest = &out[..];
    let mut start = None;
    for len in lens {
        let (message, after) = rest.split_at(len);
        start = reader.decode(Some(message)).expect("a message of the run");
        rest = after;
    }
    let start = start.expect("the last place gives the start");
    assert_eq!(start.places, places, "the run round trips");
}

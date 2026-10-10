//! The hub messages on a frame's path, and the body counters of client requests and
//! blob chunks, make no heap allocation. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use types::{
    channel,
    frame::{self, Path, Range},
};
use wire::hub::{
    Credit, FromHome, FromReader, Head, Home, Mode, Open, Reader, Reply, ends, keys,
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
    client_body();
    blob_body();
}

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
    for message in [[1].as_slice(), &out] {
        reader.decode(message).expect("the head decodes");
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
    reader.decode(&[1]).expect("the session opens");
    let (decoded, allocations) = ALLOCATOR.count(|| match reader.decode(&out) {
        Ok(FromHome::Head(decoded)) => decoded,
        other => panic!("the head did not decode: {other:?}"),
    });
    assert_eq!(allocations, 0, "the head decode allocated");
    assert_eq!(decoded, head, "the head round trips");
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
    let (last, allocations) = ALLOCATOR.count(|| match reader.decode(&run) {
        Ok(FromHome::Ends { ends, last: true }) => ends.last(),
        other => panic!("the ends did not decode: {other:?}"),
    });
    assert_eq!(allocations, 0, "the decode of {series} ends allocated");
    assert_eq!(last, Some((series - 1, series)), "the last end round trips");
    let body = vec![7; usize::try_from(series).expect("a u32 fits a usize")];
    for (index, message) in body.chunks(body.len().div_ceil(2)).enumerate() {
        let ((at, last), allocations) = ALLOCATOR.count(|| {
            let at = reader.body();
            match reader.decode(message) {
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

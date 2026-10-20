//! The seqs that a complete [`Reader`] lost, and the warning of each gap. A binary of
//! its own, as the interest of a `tracing` callsite is global to the process: a test
//! that hits the warning with no subscriber turns it off for a test beside it.

use std::ops;
use std::sync::{Arc, Mutex};

use program::Reader;
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use types::channel;
use types::frame::Range;
use types::name::Selector;
use types::sample::{Scalar, Type};
use wire::hub::client::{Mode, Place};

use crate::agent::name;
use crate::hand::{by_hand, opened_at, send_frame};
use crate::sessions::QUIET;

/// The places of a channel on `time` on each side of `time`.
fn around() -> [Place; 3] {
    let place = |at, data_type, channel| Place {
        key: channel::Key::from_u128(at),
        data_type,
        name: name(channel),
        index: 1,
    };
    let i64 = Type::Scalar(Scalar::I64);
    [
        place(10, i64, "a"),
        place(1, Type::Scalar(Scalar::Stamp), "time"),
        place(11, i64, "b"),
    ]
}

/// The level and the fields of each event of `program`, in order, as `name=value`.
/// Clones share them.
#[derive(Clone, Debug, Default)]
struct Log(Arc<Mutex<Vec<Written>>>);

/// The level of one event and its fields.
type Written = (Level, Vec<String>);

impl Log {
    fn events(&self) -> Vec<Written> {
        self.0.lock().expect("no event panics").clone()
    }
}

impl Subscriber for Log {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("program")
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        let level = *event.metadata().level();
        self.0
            .lock()
            .expect("no event panics")
            .push((level, fields.0));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

struct Fields(Vec<String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

/// One frame that [`lost`] sends: its seq, its count, and the place of each series.
type Sent = (u64, u32, &'static [u32]);

/// Opens a read of `around` in `mode` at `live`, sends each frame of `sent`, and gives
/// the `lost` of each entry of the reader before the first frame and after each, with
/// the events that the program wrote.
fn lost(
    seed: u64,
    mode: Mode,
    live: u64,
    sent: &'static [Sent],
) -> (Vec<[ops::Range<u64>; 3]>, Log) {
    let log = Log::default();
    let events = log.clone();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&kept);
    by_hand(
        seed,
        move |session, node| async move {
            let mut read = opened_at(&session, &around(), live).await;
            for &(seq, count, places) in sent {
                let len = 8 * count;
                let run: Vec<_> =
                    (1..).zip(places).map(|(n, &p)| (p, n * len)).collect();
                let body = vec![0; usize::try_from(len).expect("small") * places.len()];
                let cuts: &[usize] = if body.is_empty() { &[] } else { &[body.len()] };
                let range = Range { seq, count };
                send_frame(&mut read, range, &run, &body, cuts).await;
            }
            node.clock().sleep(QUIET).await;
        },
        move |session, _| {
            Box::pin(
                async move {
                    let selector = Selector::new(["a", "b"]).expect("a selector");
                    let mut reader =
                        session.reader(selector, mode).await.expect("opens");
                    let each =
                        |reader: &Reader| [0, 1, 2].map(|entry| reader.lost(entry));
                    let mut lost = vec![each(&reader)];
                    for _ in sent {
                        reader.next().await.expect("a frame");
                        lost.push(each(&reader));
                    }
                    *out.lock().expect("one writer") = lost;
                }
                .with_subscriber(events),
            )
        },
    );
    let lost = kept.lock().expect("one writer").clone();
    (lost, log)
}

/// The warning of the gap `start..end` of the index `time`.
fn warning(start: u64, end: u64) -> Written {
    let message = format!(
        "the reader lost seqs {start}..{end} of index time: the node's buffer did not \
         keep them"
    );
    (
        Level::WARN,
        vec![
            format!("message={message}"),
            "index=time".to_owned(),
            format!("start={start}"),
            format!("end={end}"),
        ],
    )
}

/// The lost seqs of the index alone, at entry 1.
fn index(lost: ops::Range<u64>) -> [ops::Range<u64>; 3] {
    [0..0, lost, 0..0]
}

/// A complete reader opened at 5 whose first frame starts at 9 gives the gap `5..9`
/// with that frame and writes one warning for it.
#[test]
fn gives_the_seqs_lost_before_a_frame_and_warns_once() {
    const SENT: &[Sent] = &[(9, 1, &[1]), (10, 1, &[1]), (12, 1, &[1])];
    let (lost, log) = lost(412, Mode::Complete, 5, SENT);
    let expected = [0..0, 5..9, 0..0, 11..12].map(index);
    assert_eq!(lost, expected);
    assert_eq!(log.events(), [warning(5, 9), warning(11, 12)]);
}

/// A first frame at the open position loses nothing and writes no warning.
#[test]
fn gives_no_gap_to_a_first_frame_at_the_open_position() {
    const SENT: &[Sent] = &[(5, 1, &[1]), (6, 1, &[1])];
    let (lost, log) = lost(413, Mode::Complete, 5, SENT);
    assert_eq!(lost, [0..0, 0..0, 0..0].map(index));
    assert_eq!(log.events(), []);
}

/// A channel that a gap's frame does not hold gets the gap with its next series, and
/// the gaps until then as one range. A series of no sample takes no gap.
#[test]
fn gives_each_channel_the_seqs_lost_since_its_last_series() {
    const SENT: &[Sent] = &[
        (0, 1, &[0, 1, 2]),
        (3, 1, &[1]),
        (4, 1, &[0, 1]),
        (6, 0, &[1, 2]),
        (6, 1, &[1]),
        (7, 1, &[1, 2]),
    ];
    let (lost, log) = lost(414, Mode::Complete, 0, SENT);
    let none = [0..0, 0..0, 0..0];
    assert_eq!(
        lost,
        [
            none.clone(),
            none.clone(),
            [0..0, 1..3, 0..0],
            [1..3, 0..0, 0..0],
            none.clone(),
            [0..0, 5..6, 0..0],
            [0..0, 0..0, 1..6],
        ]
    );
    assert_eq!(log.events(), [warning(1, 3), warning(5, 6)]);
}

/// A gap right after a gap gives a channel that both frames hold only the second.
#[test]
fn gives_a_gap_right_after_a_gap_alone() {
    const SENT: &[Sent] = &[(2, 1, &[0, 1]), (4, 1, &[0, 1])];
    let (lost, log) = lost(416, Mode::Complete, 0, SENT);
    let expected = [[0..0, 0..0, 0..0], [0..2, 0..2, 0..0], [3..4, 3..4, 0..0]];
    assert_eq!(lost, expected);
    assert_eq!(log.events(), [warning(0, 2), warning(3, 4)]);
}

/// A latest reader skips seqs by design: it gives no gap and writes no warning.
#[test]
fn gives_no_gap_to_a_latest_reader() {
    const SENT: &[Sent] = &[(3, 1, &[0, 1]), (9, 1, &[1, 2])];
    let (lost, log) = lost(415, Mode::Latest, 0, SENT);
    assert_eq!(lost, [0..0, 0..0, 0..0].map(index));
    assert_eq!(log.events(), []);
}

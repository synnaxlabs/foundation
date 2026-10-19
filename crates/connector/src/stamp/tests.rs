use std::future;

use hub::writer::{Config, Writer};
use proptest::prelude::*;
use types::authority::Authority;
use types::time::{Interval, Span, Stamp};

use super::Stamps;
use crate::common::{create_status, env, run_with};

/// Runs `main` on a node of `config`, with a writer on the status index of
/// `plant.stamp` and the clock of the node.
fn with_writer<T: Send + 'static>(
    config: sim::node::Config,
    main: impl AsyncFnOnce(Writer, env::clock::Clock) -> T + Send + 'static,
) -> T {
    run_with(0, config, |node, tasks| async move {
        let (hub, _) = hub::testing::open(env(&node, tasks)).await;
        let status = create_status("plant.stamp");
        hub.set_definitions(status.iter().map(|(name, def)| (name, def)));
        let config = Config {
            subject: "plant.stamp".parse().expect("a valid name"),
            authority: Authority(1),
            lease: None,
            channels: vec!["plant.stamp.status.state".parse().expect("a valid name")],
        };
        let writer = hub.writer(config).await.expect("the writer opens");
        main(writer, node.clock()).await
    })
}

#[test]
fn a_request_is_stamped_at_its_midpoint() {
    let config = sim::node::Config::default();
    let (sent, answered, output, stamp) = with_writer(config, async |writer, clock| {
        let mut stamps = Stamps::default();
        let sent = writer.now();
        let request = async {
            clock.sleep(Span::from_nanos(3_000)).await;
            7
        };
        let (output, stamp) = stamps.request(&writer, request).await;
        (sent, writer.now(), output, stamp)
    });
    assert_eq!(answered - sent, Span::from_nanos(3_000));
    assert_eq!(output, 7);
    assert_eq!(stamp, sent + Span::from_nanos(1_500));
}

#[test]
fn a_request_with_no_wait_is_stamped_after_the_one_before() {
    let (now, stamps) = with_writer(sim::node::Config::default(), async |writer, _| {
        let mut stamps = Stamps::default();
        let mut given = Vec::new();
        for _ in 0..3 {
            given.push(stamps.request(&writer, future::ready(())).await.1);
        }
        (writer.now(), given)
    });
    let after = |n| now + Span::from_nanos(n);
    assert_eq!(stamps, [now, after(1), after(2)]);
}

#[test]
fn a_request_after_a_refusal_is_stamped_after_its_stamp() {
    let (now, refused, stamp) =
        with_writer(sim::node::Config::default(), async |writer, _| {
            let mut stamps = Stamps::default();
            let now = writer.now();
            let refused = stamps.after(now + Span::from_nanos(20));
            let ((), stamp) = stamps.request(&writer, future::ready(())).await;
            (now, refused, stamp)
        });
    assert_eq!(refused, now + Span::from_nanos(21));
    assert_eq!(stamp, now + Span::from_nanos(22));
}

#[test]
fn a_request_near_the_end_of_time_is_stamped_at_its_midpoint() {
    let wall = Stamp::from_nanos(i64::MAX - 100 * Span::SECOND.nanos());
    let config = sim::node::Config {
        wall,
        ..Default::default()
    };
    let (sent, stamp) = with_writer(config, async |writer, clock| {
        let mut stamps = Stamps::default();
        let sent = writer.now();
        let request = clock.sleep(Span::from_nanos(2 * Span::SECOND.nanos() + 1));
        (sent, stamps.request(&writer, request).await.1)
    });
    assert_eq!(stamp, sent + Span::SECOND);
}

#[test]
fn a_request_before_the_epoch_is_stamped_at_its_midpoint_rounded_down() {
    let config = sim::node::Config {
        wall: Stamp::from_nanos(-100 * Span::SECOND.nanos()),
        ..Default::default()
    };
    let (sent, stamp) = with_writer(config, async |writer, clock| {
        let mut stamps = Stamps::default();
        let sent = writer.now();
        let request = clock.sleep(Span::from_nanos(2 * Span::SECOND.nanos() + 1));
        (sent, stamps.request(&writer, request).await.1)
    });
    assert!(sent.nanos() < 0, "{sent:?}");
    assert_eq!(stamp, sent + Span::SECOND);
}

#[test]
#[should_panic(expected = "stamp overflow: 9223372036854775807 ns + 1 ns")]
fn a_stamp_after_the_last_instant_panics() {
    let mut stamps = Stamps::default();
    stamps.next(Stamp::from_nanos(i64::MAX));
    stamps.next(Stamp::from_nanos(0));
}

#[test]
#[should_panic(expected = "stamp overflow: 9223372036854775807 ns + 1 ns")]
fn a_refusal_at_the_last_instant_panics() {
    Stamps::default().after(Stamp::from_nanos(i64::MAX));
}

#[test]
#[should_panic(expected = "refusal under the last stamp: 9 ns < 11 ns")]
fn a_refusal_under_the_last_stamp_panics() {
    let mut stamps = Stamps::default();
    stamps.after(Stamp::from_nanos(10));
    stamps.after(Stamp::from_nanos(9));
}

#[test]
fn a_refusal_at_the_last_stamp_is_stamped_after_it() {
    let mut stamps = Stamps::default();
    stamps.after(Stamp::from_nanos(10));
    assert_eq!(stamps.after(Stamp::from_nanos(11)), Stamp::from_nanos(12));
}

fn interval(earliest: i64, latest: i64) -> Interval {
    Interval {
        earliest: Stamp::from_nanos(earliest),
        latest: Stamp::from_nanos(latest),
    }
}

fn sample(stamps: &mut Stamps, time: Interval, held: i64) -> (i64, i64) {
    let (stamp, error) = stamps.sample(time, Stamp::from_nanos(held));
    (stamp.nanos(), error.nanos())
}

#[test]
fn a_sample_is_stamped_at_its_midpoint_within_half_its_width() {
    let first = |time| sample(&mut Stamps::default(), time, 0);
    assert_eq!(first(interval(100, 201)), (150, 51));
    assert_eq!(first(interval(-201, -100)), (-151, 51));
    assert_eq!(first(interval(300, 300)), (300, 0));
}

#[test]
fn a_sample_stamped_after_the_last_stamp_adds_how_far_it_is_late() {
    let mut stamps = Stamps::default();
    assert_eq!(sample(&mut stamps, interval(100, 200), 0), (150, 50));
    assert_eq!(sample(&mut stamps, interval(100, 140), 0), (151, 51));
}

#[test]
fn a_sample_held_past_its_stamp_adds_nothing() {
    let mut stamps = Stamps::default();
    sample(&mut stamps, interval(100, 200), 0);
    assert_eq!(sample(&mut stamps, interval(100, 140), 152), (151, 20));
    assert_eq!(sample(&mut stamps, interval(100, 140), 152), (152, 20));
    assert_eq!(sample(&mut stamps, interval(100, 140), 140), (153, 33));
}

#[test]
fn a_reversed_sample_interval_gives_the_same_stamp() {
    let mut stamps = Stamps::default();
    assert_eq!(sample(&mut stamps, interval(201, 100), 0), (150, 51));
}

#[test]
fn the_widest_sample_interval_saturates_the_error() {
    let mut stamps = Stamps::default();
    let widest = interval(i64::MIN, i64::MAX);
    assert_eq!(sample(&mut stamps, widest, 0), (-1, i64::MAX));
    let late = interval(i64::MAX - 1, i64::MAX - 1);
    assert_eq!(sample(&mut stamps, late, i64::MIN), (i64::MAX - 1, 0));
}

#[test]
#[should_panic(expected = "stamp overflow: 9223372036854775807 ns + 1 ns")]
fn a_sample_after_the_last_instant_panics() {
    let mut stamps = Stamps::default();
    let last = interval(i64::MAX, i64::MAX);
    sample(&mut stamps, last, 0);
    sample(&mut stamps, last, 0);
}

/// A sample: its true time `at` within `time`, and the time its value holds until.
#[derive(Clone, Debug)]
struct Sample {
    time: Interval,
    at: i64,
    held: i64,
}

fn sample_input() -> impl Strategy<Value = Sample> {
    (0..1_000i64, 0..100i64, 0..=100u8, -50..200i64).prop_map(
        |(earliest, width, part, held)| {
            let at = earliest + width * i64::from(part) / 100;
            Sample {
                time: interval(earliest, earliest + width),
                at,
                held: at + held,
            }
        },
    )
}

/// A call of [`Stamps`]: a stamp at some time, or a refusal whose stamp before is
/// some time after the last stamp.
#[derive(Clone, Debug)]
enum Call {
    Next { time: i64 },
    After { ahead: i64 },
}

fn call() -> impl Strategy<Value = Call> {
    prop_oneof![
        3 => (0..1_000i64).prop_map(|time| Call::Next { time }),
        1 => (0..1_000i64).prop_map(|ahead| Call::After { ahead }),
    ]
}

proptest! {
    /// In any order of calls, each stamp is after the one before it. A stamp is at
    /// its time when that is later, and a refusal gives the nanosecond after its
    /// stamp before. It calls `next`, which the status writer calls, so it needs no
    /// hub.
    #[test]
    fn stamps_strictly_increase_from_their_times(
        calls in prop::collection::vec(call(), 1..50),
    ) {
        let mut stamps = Stamps::default();
        let mut last = None::<i64>;
        for call in calls {
            let (stamp, expected) = match call {
                Call::Next { time } => (
                    stamps.next(Stamp::from_nanos(time)),
                    last.map_or(time, |last| time.max(last + 1)),
                ),
                Call::After { ahead } => {
                    let before = last.unwrap_or(0) + ahead;
                    (stamps.after(Stamp::from_nanos(before)), before + 1)
                }
            };
            prop_assert_eq!(stamp, Stamp::from_nanos(expected));
            prop_assert!(last.is_none_or(|last| stamp.nanos() > last));
            last = Some(stamp.nanos());
        }
    }

    /// Each stamp of a sample is after the one before it, and within its error of a
    /// time at which its value held: from its true time to `held`.
    #[test]
    fn sample_stamps_increase_and_their_errors_hold_the_value(
        samples in prop::collection::vec(sample_input(), 1..50),
    ) {
        let mut stamps = Stamps::default();
        let mut last = None::<i64>;
        for Sample { time, at, held } in samples {
            let (stamp, error) = sample(&mut stamps, time, held);
            prop_assert!(last.is_none_or(|last| stamp > last));
            let nearest = stamp.clamp(at, at.max(held));
            prop_assert!(
                stamp.abs_diff(nearest) <= error.unsigned_abs(),
                "stamp {stamp} ns, error {error} ns, value from {at} to {held} ns"
            );
            last = Some(stamp);
        }
    }
}

use std::future;

use hub::home::{Outcome, Refusal, order};
use hub::writer::{Config, Failure, Writer};
use proptest::prelude::*;
use spec::definition::Definition;
use types::authority::Authority;
use types::channel;
use types::frame::{self, Draft, Form, Label, Path};
use types::name::Name;
use types::time::{Interval, Span, Stamp};

use super::{Stamps, backwards_again};
use crate::common::{create_status, env, run_with};
use crate::{stamp, testing};

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

/// Runs `main` with the hub of a node that defines the status channels of `plant.a`
/// and of `plant.b`.
fn with_hub<T: Send + 'static>(
    main: impl AsyncFnOnce(hub::Hub) -> T + Send + 'static,
) -> T {
    run_with(0, sim::node::Config::default(), |node, tasks| async move {
        let (hub, _) = hub::testing::open(env(&node, tasks)).await;
        hub.set_definitions(status(&["a", "b"]).iter().map(|(name, def)| (name, def)));
        main(hub).await
    })
}

/// The status channels of each of `plants`, such as `a` for `plant.a`, with the keys
/// 100 on for `a` and 200 on for `b`.
fn status(plants: &[&str]) -> Vec<(Name, Definition)> {
    (plants.iter())
        .flat_map(|&plant| {
            let first = if plant == "a" { 100 } else { 200 };
            let name = format!("plant.{plant}").parse().expect("a valid name");
            testing::create_status(&name, &[], channel::Key::from_u128(first))
        })
        .collect()
}

/// Opens a writer at `authority` of `channels`, each a plant and the last part of a
/// status channel, such as `a.state` for `plant.a.status.state`.
async fn open(hub: &hub::Hub, authority: u8, channels: &[&str]) -> Writer {
    let channels = (channels.iter())
        .map(|channel| {
            let (plant, last) = channel.split_once('.').expect("a plant and a channel");
            let name = format!("plant.{plant}.status.{last}");
            name.parse().expect("a valid name")
        })
        .collect();
    let config = Config {
        subject: "plant".parse().expect("a valid name"),
        authority: Authority(authority),
        lease: None,
        channels,
    };
    hub.writer(config).await.expect("the writer opens")
}

/// Writes a frame of `channel` `ahead` of the mesh time with a writer at authority 2,
/// over the writers of a test, that it then closes, and gives the stamp of that frame.
async fn write_ahead(hub: &hub::Hub, channel: &str, ahead: Span) -> Stamp {
    let mut rival = open(hub, 2, &[channel]).await;
    let ahead = rival.now() + ahead;
    assert_eq!(put(&mut rival, ahead), None, "the frame ahead applies");
    ahead
}

/// Writes one frame at `stamp` on `writer`, with 7 in its data channel, and gives the
/// stamp that the home gives when it refuses the frame as `Backwards`.
fn put(writer: &mut Writer, stamp: Stamp) -> Option<Stamp> {
    let set = writer.set();
    let series: Vec<_> = (set.entries().iter().enumerate())
        .map(|(i, entry)| (i, entry.data_type.width().expect("a fixed width")))
        .collect();
    let group = set.entries()[0].group;
    let index: Vec<_> = (0..series.len()).map(|i| set.index(i) == i).collect();
    let mut draft = writer.draft(Form::Raw, &series).expect("a block");
    for (i, bytes) in draft.iter_mut() {
        let value = if index[i] {
            stamp.nanos().cast_unsigned()
        } else {
            7
        };
        let len = bytes.len();
        bytes.copy_from_slice(&value.to_le_bytes()[..len]);
    }
    draft.set_count(group, 1);
    match writer.write(Label::Path(Path::Live), draft) {
        Ok([Outcome::Applied { .. }]) => None,
        Ok(
            [
                Outcome::Refused {
                    refusal: Refusal::Order(order::Error::Backwards { before, .. }),
                    ..
                },
            ],
        ) => Some(*before),
        other => panic!("the frame is applied or backwards: {other:?}"),
    }
}

/// Each entry of a frame of `writer`, with the width of its type.
fn series(writer: &stamp::Writer) -> Vec<(usize, usize)> {
    (writer.hub().set().entries().iter().enumerate())
        .map(|(entry, e)| (entry, e.data_type.width().expect("a fixed width")))
        .collect()
}

/// The stamp and the entry of each call of the fill of [`stamp::Writer::write`].
type Fills = Vec<(Stamp, usize)>;

/// What [`stamp::Writer::write`] gives, owned.
type Wrote = Result<Vec<(u32, bool)>, stamp::Error>;

/// Writes a frame of each entry at the next stamp with `writer`, with 7 in each data
/// entry, and gives the result, the stamp, and each fill.
fn write(writer: &mut stamp::Writer) -> (Wrote, Stamp, Fills) {
    write_series(writer, &series(writer))
}

/// As [`write`], with the entries and lengths of `series`.
fn write_series(
    writer: &mut stamp::Writer,
    series: &[(usize, usize)],
) -> (Wrote, Stamp, Fills) {
    let stamp = writer.next(writer.hub().now());
    let mut fills = Fills::new();
    let wrote = writer.write(stamp, series, |stamp, entry, bytes| {
        fills.push((stamp, entry));
        let len = bytes.len();
        bytes.copy_from_slice(&7_u64.to_le_bytes()[..len]);
    });
    (wrote.map(<[_]>::to_vec), stamp, fills)
}

/// The group of each entry of the frames of `writer`.
fn groups(writer: &stamp::Writer) -> Vec<u32> {
    writer
        .hub()
        .set()
        .entries()
        .iter()
        .map(|entry| entry.group)
        .collect()
}

#[test]
fn write_writes_a_frame_refused_as_backwards_again_1_ns_after_the_refusal() {
    let (ahead, (wrote, stamp, fills), groups, next) = with_hub(async |hub| {
        let ahead = write_ahead(&hub, "a.restarts", Span::MILLISECOND).await;
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        let wrote = write(&mut writer);
        (
            ahead,
            wrote,
            groups(&writer),
            writer.next(Stamp::from_nanos(0)),
        )
    });
    assert!(stamp < ahead, "{stamp} < {ahead}");
    assert_eq!(wrote, Ok(vec![(groups[0], true)]));
    let after = |n| ahead + Span::from_nanos(n);
    assert_eq!(fills, [(stamp, 1), (after(1), 1)]);
    assert_eq!(next, after(2));
}

#[test]
fn write_writes_a_frame_once_when_the_home_applies_it() {
    let ((wrote, stamp, fills), groups) = with_hub(async |hub| {
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        (write(&mut writer), groups(&writer))
    });
    assert_eq!(wrote, Ok(vec![(groups[0], true)]));
    assert_eq!(fills, [(stamp, 1)]);
}

#[test]
fn write_writes_again_only_the_series_of_the_group_refused_as_backwards() {
    let (ahead, (wrote, stamp, fills), groups, keys) = with_hub(async |hub| {
        let ahead = write_ahead(&hub, "a.restarts", Span::MILLISECOND).await;
        let channels = ["a.state", "b.state"];
        let mut writer = stamp::Writer::new(open(&hub, 1, &channels).await);
        let keys: Vec<_> = (writer.hub().set().entries().iter())
            .map(|entry| entry.key.as_u128())
            .collect();
        (ahead, write(&mut writer), groups(&writer), keys)
    });
    let at = |key| {
        keys.iter()
            .position(|&k| k == key)
            .expect("the key is in the set")
    };
    let (a, b) = (at(101), at(201));
    let mut applied = vec![(groups[a], true), (groups[b], true)];
    applied.sort_unstable();
    assert_eq!(wrote, Ok(applied));
    let mut first = vec![(stamp, a), (stamp, b)];
    first.sort_unstable_by_key(|&(_, entry)| entry);
    first.push((ahead + Span::from_nanos(1), a));
    assert_eq!(fills, first);
}

#[test]
fn write_writes_again_only_the_group_refused_in_this_write() {
    let (ahead, (wrote, stamp, fills), groups, keys) = with_hub(async |hub| {
        write_ahead(&hub, "a.restarts", Span::MILLISECOND).await;
        let channels = ["a.state", "b.state"];
        let mut writer = stamp::Writer::new(open(&hub, 1, &channels).await);
        let keys: Vec<_> = (writer.hub().set().entries().iter())
            .map(|entry| entry.key.as_u128())
            .collect();
        let (first, ..) = write(&mut writer);
        assert!(first.is_ok(), "{first:?}");
        // After the stamp of the write again of group a.
        let ahead = write_ahead(&hub, "b.restarts", Span::from_nanos(2_000_000)).await;
        (ahead, write(&mut writer), groups(&writer), keys)
    });
    let at = |key| {
        keys.iter()
            .position(|&k| k == key)
            .expect("the key is in the set")
    };
    let (a, b) = (at(101), at(201));
    let mut applied = vec![(groups[a], true), (groups[b], true)];
    applied.sort_unstable();
    assert_eq!(wrote, Ok(applied));
    let mut second = vec![(stamp, a), (stamp, b)];
    second.sort_unstable_by_key(|&(_, entry)| entry);
    second.push((ahead + Span::from_nanos(1), b));
    assert_eq!(fills, second);
}

/// The address and the capacity of each buffer of `writer`. A write that grows a
/// buffer changes them.
fn buffers(writer: &stamp::Writer) -> [(usize, usize); 4] {
    [
        (writer.groups.as_ptr().addr(), writer.groups.capacity()),
        (
            writer.backwards.as_ptr().addr(),
            writer.backwards.capacity(),
        ),
        (writer.again.as_ptr().addr(), writer.again.capacity()),
        (writer.retried.as_ptr().addr(), writer.retried.capacity()),
    ]
}

#[test]
fn write_grows_no_buffer_when_it_writes_again() {
    let (one, two) = with_hub(async |hub| {
        write_ahead(&hub, "a.restarts", Span::MILLISECOND).await;
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        let before = buffers(&writer);
        let (wrote, _, fills) = write(&mut writer);
        assert_eq!(fills.len(), 2, "the frame goes again: {wrote:?}");
        let one = (before, buffers(&writer));
        drop(writer);
        write_ahead(&hub, "a.restarts", Span::from_nanos(2_000_000)).await;
        let channels = ["a.state", "b.state"];
        let mut writer = stamp::Writer::new(open(&hub, 1, &channels).await);
        let before = buffers(&writer);
        let mut after = Vec::new();
        for (ahead, channel) in [(4, "b.restarts"), (6, "a.restarts")] {
            let (wrote, _, fills) = write(&mut writer);
            assert_eq!(fills.len(), 3, "one group goes again: {wrote:?}");
            after.push(buffers(&writer));
            write_ahead(&hub, channel, Span::from_nanos(ahead * 1_000_000)).await;
        }
        (one, (before, after))
    });
    assert_eq!(one.1, one.0, "one group");
    assert_eq!(two.1, [two.0; 2], "two groups, refused one at a time");
}

#[test]
fn write_loses_a_frame_when_the_pool_has_no_block_now() {
    let ((wrote, _, fills), groups) = with_hub(async |hub| {
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        let held = hold(writer.hub());
        let wrote = write(&mut writer);
        drop(held);
        (wrote, groups(&writer))
    });
    assert_eq!(wrote, Ok(vec![(groups[0], false)]));
    assert_eq!(fills, []);
}

/// Drafts frames of `writer` until the pool has no block, and gives them.
fn hold(writer: &Writer) -> Vec<Draft> {
    let mut held = Vec::new();
    for len in [1 << 16, 1 << 12, 1 << 8, 8] {
        let series: Vec<_> = (0..writer.set().entries().len())
            .map(|entry| (entry, len))
            .collect();
        while let Ok(draft) = writer.draft(Form::Raw, &series) {
            held.push(draft);
        }
    }
    held
}

#[test]
fn write_gives_the_error_of_a_frame_larger_than_the_largest_block() {
    let ((wrote, _, fills), large) = with_hub(async |hub| {
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        let series = [(0, 8), (1, 1 << 40)];
        let large = writer.hub().draft(Form::Raw, &series).err();
        (write_series(&mut writer, &series), large)
    });
    let large = large.expect("no block holds the frame");
    assert!(
        matches!(large, frame::Error::Pool(block::Error::TooLarge { .. })),
        "{large:?}"
    );
    assert_eq!(wrote, Err(stamp::Error::Draft(large)));
    assert_eq!(fills, []);
}

#[test]
fn write_loses_a_frame_of_a_channel_that_a_higher_authority_controls() {
    let ((wrote, stamp, fills), groups) = with_hub(async |hub| {
        let rival = open(&hub, 2, &["a.state"]).await;
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        let wrote = write(&mut writer);
        drop(rival);
        (wrote, groups(&writer))
    });
    assert_eq!(wrote, Ok(vec![(groups[0], false)]));
    assert_eq!(fills, [(stamp, 1)]);
}

#[test]
fn write_gives_the_failure_of_the_hub_when_a_channel_is_removed() {
    let ((wrote, stamp, fills), key) = with_hub(async |hub| {
        let mut writer = stamp::Writer::new(open(&hub, 1, &["a.state"]).await);
        hub.set_definitions(status(&["b"]).iter().map(|(name, def)| (name, def)));
        let key = writer.hub().set().entries()[1].key;
        (write(&mut writer), key)
    });
    assert_eq!(wrote, Err(stamp::Error::Write(Failure::Removed(key))));
    assert_eq!(fills, [(stamp, 1)]);
}

/// No caller reaches this panic: the two sends of `stamp::Writer::write` run with no
/// await between them, so no rival writes ahead of the stamp of the write again.
#[test]
#[should_panic(expected = "invariant: a frame stamped after \
                           1970-01-01T00:00:00.000000009Z, the last stamp of its \
                           index, is not backwards, but the home gives \
                           1970-01-01T00:00:00.000000012Z as last")]
fn backwards_again_names_both_stamps() {
    backwards_again(Stamp::from_nanos(9), Stamp::from_nanos(12));
}

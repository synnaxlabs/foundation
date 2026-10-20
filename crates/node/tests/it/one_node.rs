//! The exit tests of ONE NODE: one `foundation` node reads a simulated OPC UA server
//! and pushes each sample to a simulated InfluxDB. Exit test 1 runs on the real binary
//! with `cargo test -p node --test it samples_from_an_opc_ua_server_reach_influxdb`.

mod sim;

use std::collections::BTreeMap;
use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;

use types::quality::Quality;
use types::time::{Span, Stamp};

use self::sim::{Influx, Opcua, Sample};
use crate::quickstart::plant;
use crate::rig::Rig;
use crate::status::{Connector, Error};
use crate::text;

/// The channel `plant.spike`, with the quality channel `plant.quality`.
const QUALITY: &str = include_str!("one_node/quality.hcl");

/// Each variable of the OPC UA server, and the channel that reads it in each config.
const VARIABLES: [(&str, &str); 3] = [
    ("ns=3;s=SpikeData", "plant.spike"),
    ("ns=3;s=DipData", "plant.dip"),
    ("ns=3;s=PositiveTrendData", "plant.trend"),
];

/// The changes of each variable. Each change steps the value.
const TICKS: usize = 20;

/// The changes of each variable in the test of the time bound: 5 s at 10 Hz, so that a
/// time error that grows slowly passes [`BOUND`].
const TRACED: usize = 50;

/// The MVP target of the time error bound.
const BOUND: Span = Span::SECOND;

/// `Bad`, with no subcode.
const BAD: Quality = Quality(0x8000_0000);

/// `Uncertain`, with no subcode.
const UNCERTAIN: Quality = Quality(0x4000_0000);

/// A node, the OPC UA server that it reads, and the InfluxDB that it pushes to.
#[derive(Debug)]
struct Plant {
    rig: Rig,
    opcua: Opcua,
    influx: Influx,
    /// The time of `opcua` before the node started.
    started: Stamp,
}

impl Plant {
    /// Starts the node of `rig`, which reads `opcua` and pushes to `influx`. No config
    /// applies yet.
    fn start(mut rig: Rig, opcua: Opcua, influx: Influx) -> Self {
        let started = opcua.now();
        rig.start();
        Self {
            rig,
            opcua,
            influx,
            started,
        }
    }
}

/// A running node on `hcl`, with both servers serving.
fn create_plant(hcl: &str) -> Plant {
    let mut opcua = Opcua::default();
    let mut influx = Influx::default();
    let rig = Rig::new();
    rig.config(&served(hcl, opcua.serve(), influx.serve()));
    let plant = Plant::start(rig, opcua, influx);
    plant.rig.apply();
    plant
}

/// The quickstart config, with the token of `influx` from `source`.
fn with_token(source: &str) -> String {
    let select = "  select      = \"plant.*\"\n";
    let hcl =
        plant().replacen(select, &format!("{select}  token       = {source}\n"), 1);
    assert_ne!(hcl, plant(), "the quickstart config selects `plant.*`");
    hcl
}

/// A running node on `edit` of the config with the token that `source` gives, with
/// `INFLUX_TOKEN` set to `t0ken`, which pushes to an InfluxDB that takes only
/// `t0ken`. No config applies yet.
fn create_token_plant(
    source: impl FnOnce(&Rig) -> String,
    edit: fn(&str) -> String,
) -> Plant {
    let mut opcua = Opcua::default();
    let mut influx = Influx::taking("t0ken");
    let mut rig = Rig::new();
    rig.var("INFLUX_TOKEN", "t0ken");
    let hcl = edit(&with_token(&source(&rig)));
    rig.config(&served(&hcl, opcua.serve(), influx.serve()));
    Plant::start(rig, opcua, influx)
}

/// Asserts that `plan` of `rig` exits with one `secret.unresolved` problem at the
/// token of [`with_token`], with `message`, and writes no plan.
fn refuses_the_token(rig: &Rig, message: &str) {
    let hcl = std::fs::read_to_string(rig.dir.join("plant.hcl")).expect("reads it");
    let line = 1
        + (hcl.lines())
            .position(|line| line.starts_with("  token "))
            .expect("the config has a token");
    let output = rig.run(&["plan", "plant.hcl", "--out", "plant.plan"], b"");
    let expected = format!(
        "error[secret.unresolved]: {message}\n  --> plant.hcl:{line}:17\nfix: Give the \
         node the value at the source, or change the source, then plan again\n"
    );
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (Some(2), "", &*expected)
    );
    assert!(!rig.dir.join("plant.plan").exists(), "plan writes no plan");
}

/// The error that `influx` shows, once it shows one.
fn influx_error(rig: &Rig) -> Error {
    rig.wait("`influx` shows an error", || {
        let status = rig.status();
        (status.get("influx").and_then(|influx| influx.error.clone()))
            .ok_or_else(|| format!("{status:#?}"))
    })
}

/// `hcl` with `opcua` in place of `localhost:50000`, and `influx` in place of
/// `localhost:8086`.
fn served(hcl: &str, opcua: SocketAddr, influx: SocketAddr) -> String {
    (hcl.replace("localhost:50000", &opcua.to_string()))
        .replace("localhost:8086", &influx.to_string())
}

/// `hcl` with its OPC UA connector polled, so that it reads each node at each tick, as
/// the trace of [`held`] and a count that grows while the nodes do not change ask.
/// Until #3537, a sink that opens after the first value of a node that does not change
/// gets no sample of it, so a sink that starts late also asks for it.
fn polled(hcl: &str) -> String {
    let rate = "  rate    = \"10Hz\"\n";
    let polled = hcl.replacen(rate, &format!("{rate}  polled  = true\n"), 1);
    assert_ne!(polled, hcl, "the config has an OPC UA connector at 10 Hz");
    polled
}

/// The reads that the server answered of one variable, from the first to the end of
/// the changes, and the samples that InfluxDB holds of them to the last step of the
/// value. A later read can stand in for the read of the last step: only a change
/// steps the value and sets the status, so it has the same status.
#[derive(Debug)]
struct Trace {
    served: Vec<Sample>,
    stored: Vec<Sample>,
    /// How many of `served` the server answered before the changes.
    unchanged: usize,
}

/// How many reads of one variable the server answered before and to the end of the
/// changes.
#[derive(Debug)]
struct Reads {
    variable: &'static str,
    channel: &'static str,
    unchanged: usize,
    changed: usize,
}

/// Waits until InfluxDB holds a sample of each channel of `variables`.
fn first_stored(plant: &Plant, variables: &[(&str, &str)]) {
    plant.rig.wait("InfluxDB holds the first values", || {
        let stored = plant.influx.stored();
        let empty: Vec<_> = variables
            .iter()
            .map(|&(_, channel)| channel)
            .filter(|channel| stored.get(*channel).is_none_or(Vec::is_empty))
            .collect();
        if empty.is_empty() {
            Ok(())
        } else {
            Err(format!("no sample of {empty:?}"))
        }
    });
}

/// Changes each variable of `variables` `ticks` times with `quality`, and gives the
/// reads of each. Panics when the server answered fewer than half of `ticks` reads of a
/// variable in the changes, so that a node that reads too few at `rate = "10Hz"` fails.
fn change(
    opcua: &Opcua,
    variables: &[(&'static str, &'static str)],
    quality: Quality,
    ticks: usize,
) -> Vec<Reads> {
    let count = |served: &BTreeMap<String, Vec<Sample>>, variable| {
        served.get(variable).map_or(0, Vec::len)
    };
    let before = opcua.served();
    opcua.change(ticks, quality);
    let after = opcua.served();
    let reads: Vec<_> = variables
        .iter()
        .map(|&(variable, channel)| Reads {
            variable,
            channel,
            unchanged: count(&before, variable),
            changed: count(&after, variable),
        })
        .collect();
    for reads in &reads {
        let in_changes = reads.changed - reads.unchanged;
        let channel = reads.channel;
        assert!(
            in_changes >= ticks / 2,
            "{channel}: {in_changes} reads in the changes"
        );
    }
    reads
}

/// Waits until InfluxDB holds each of `reads`, and gives the trace of each.
fn held(plant: &Plant, reads: &[Reads]) -> Vec<Trace> {
    plant.rig.wait("InfluxDB holds each read", || {
        let stored = plant.influx.stored();
        caught_up(reads, plant.started, plant.opcua.served(), stored)
    })
}

/// The trace of each of `reads`, once `stored` (by channel) holds each read of
/// `served` (by variable) to the last step of the value in the changes, and `served`
/// holds the read after that step. Sample `k` stored is read `k`, as [`traced`] checks.
/// A lost or extra sample before a step puts a sample on a read of another value. From
/// the last step on, a later read of the same value can stand in for a lost read. The
/// node started at `started`, and `served` is taken after `stored`. Else `Err` with
/// what `served` or `stored` lacks, or with the first sample that is not its read and
/// the offset and window of each checked sample.
fn caught_up(
    reads: &[Reads],
    started: Stamp,
    mut served: BTreeMap<String, Vec<Sample>>,
    mut stored: BTreeMap<String, Vec<Sample>>,
) -> Result<Vec<Trace>, String> {
    reads
        .iter()
        .map(|reads| {
            let channel = reads.channel;
            let served = served.remove(reads.variable).unwrap_or_default();
            let stored = stored.remove(channel).unwrap_or_default();
            let end = (1..reads.changed)
                .rev()
                .find(|&k| served[k].value.to_bits() != served[k - 1].value.to_bits())
                .unwrap_or_else(|| {
                    panic!(
                        "{channel}: no read of the changes steps the value: {:?}",
                        &served[..reads.changed]
                    )
                });
            let to_end = &stored[..stored.len().min(end + 1)];
            traced(started, &served, to_end)
                .map_err(|error| format!("{channel}: {error}"))?;
            if stored.len() <= end {
                return Err(format!(
                    "{channel}: InfluxDB holds {} of the {} reads",
                    stored.len(),
                    end + 1
                ));
            }
            Ok(Trace {
                served: served[..reads.changed].to_vec(),
                stored: stored[..=end].to_vec(),
                unchanged: reads.unchanged,
            })
        })
        .collect()
}

/// Whether sample `k` of `stored` is read `k` of `served`: read `k` is Good, the sample
/// has its value, and its time is [`within`] the limit of read `k`. InfluxDB holds no
/// status, so a sample is Good when its read is. The node started at `started`. Else
/// `Err` with the first sample that is not its read and the offset and window of each
/// sample, or with the last read of `served` when `stored` holds a sample of it: its
/// window needs the read after it.
fn traced(started: Stamp, served: &[Sample], stored: &[Sample]) -> Result<(), String> {
    let last = served.len().saturating_sub(1);
    if stored.len() > last {
        return Err(format!("the server answered no read after read {last}"));
    }
    let checked: Vec<_> = (stored.iter().zip(served).zip(windows(started, served)))
        .map(|((sample, read), window)| {
            (sample, read, sample.time.checked_since(read.time), window)
        })
        .collect();
    for (k, &(sample, read, offset, window)) in checked.iter().enumerate() {
        if read.quality != Some(Quality::GOOD)
            || sample.value.to_bits() != read.value.to_bits()
            || !within(k, offset, window)
        {
            let offsets = checked.iter().map(|&(.., offset, window)| (offset, window));
            return Err(format!(
                "sample {k} stored, {sample:?}, is not read {k}, {read:?}\nthe offset \
                 of each sample in its window: {}",
                shown(offsets)
            ));
        }
    }
    Ok(())
}

/// Whether a sample `offset` from read `k` is within its limit. The window of a read is
/// the time from the read before it, or from the start of the node, to the read after
/// it. A stamp is the midpoint of its read, so the hold, half the window plus
/// [`BOUND`], holds each sample. A read after the first whose window is at most
/// `2 * BOUND` takes the MVP target, [`BOUND`]. An offset that does not fit in a
/// [`Span`] is over each limit.
fn within(k: usize, offset: Option<Span>, window: Span) -> bool {
    let Some(offset) = offset else {
        return false;
    };
    let offset = i128::from(offset.nanos()).abs();
    let (window, bound) = (i128::from(window.nanos()), i128::from(BOUND.nanos()));
    if k >= 1 && window <= 2 * bound {
        offset <= bound
    } else {
        2 * offset <= window + 2 * bound
    }
}

/// Each offset and its window, as `0: 5ms in 200ms, 1: 1.005s in 200ms`.
fn shown(offsets: impl Iterator<Item = (Option<Span>, Span)>) -> String {
    let shown: Vec<_> = (offsets.enumerate())
        .map(|(k, (offset, window))| match offset {
            Some(offset) => format!("{k}: {offset} in {window}"),
            None => format!("{k}: out of range in {window}"),
        })
        .collect();
    shown.join(", ")
}

/// The window of each read of `served` but the last, from a node that started at
/// `started`.
fn windows(started: Stamp, served: &[Sample]) -> impl Iterator<Item = Span> + '_ {
    let before = std::iter::once(started).chain(served.iter().map(|read| read.time));
    before
        .zip(served.iter().skip(1))
        .map(|(before, after)| after.time - before)
}

/// The count `key` of the connector `name` in `status`.
fn count(status: &BTreeMap<String, Connector>, name: &str, key: &str) -> Option<u64> {
    status.get(name)?.counts.as_ref()?.get(key).copied()
}

/// `in` of `plc` and `confirmed` of `influx`, once both are above `floor`.
fn counts_above(rig: &Rig, floor: (u64, u64)) -> Result<(u64, u64), String> {
    let status = rig.status();
    match (
        count(&status, "plc", "in"),
        count(&status, "influx", "confirmed"),
    ) {
        (Some(read), Some(confirmed)) if read > floor.0 && confirmed > floor.1 => {
            Ok((read, confirmed))
        }
        _ => Err(format!("{status:#?}")),
    }
}

/// Three reads of `plant.spike` before the changes, three in them, and one after the
/// last change, at 10 Hz.
fn create_served() -> Vec<Sample> {
    let values = [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 3.0];
    (10..)
        .zip(values)
        .map(|(at, value)| Sample {
            time: Stamp::from_nanos(at * 100 * Span::MILLISECOND.nanos()),
            value,
            quality: Some(Quality::GOOD),
        })
        .collect()
}

/// [`caught_up`] of `stored` against `served`, from a node that started at 0.9 s, one
/// read before the first of [`create_served`].
fn check(served: &[Sample], stored: &[Sample]) -> Result<Vec<Trace>, String> {
    let reads = [Reads {
        variable: "ns=3;s=SpikeData",
        channel: "plant.spike",
        unchanged: 3,
        changed: 6,
    }];
    caught_up(
        &reads,
        Stamp::from_nanos(900 * Span::MILLISECOND.nanos()),
        BTreeMap::from([("ns=3;s=SpikeData".into(), served.to_vec())]),
        BTreeMap::from([("plant.spike".into(), stored.to_vec())]),
    )
}

/// `samples`, each `by` later.
fn later(samples: &[Sample], by: Span) -> Vec<Sample> {
    let later = |sample: &Sample| Sample {
        time: sample.time + by,
        ..*sample
    };
    samples.iter().map(later).collect()
}

/// `samples`, with sample `k` `by` later.
fn moved(samples: &[Sample], k: usize, by: Span) -> Vec<Sample> {
    let mut moved = samples.to_vec();
    moved[k].time = moved[k].time + by;
    moved
}

/// [`create_served`], with each read after read `k` later, so that the window of read
/// `k` is `window`.
fn widened(k: usize, window: Span) -> Vec<Sample> {
    let served = create_served();
    let by = window.nanos() - (served[k + 1].time - served[k - 1].time).nanos();
    let (before, after) = served.split_at(k + 1);
    [before, &later(after, Span::from_nanos(by))].concat()
}

/// `by` and `-by`.
fn both(by: Span) -> [Span; 2] {
    [by, Span::from_nanos(-by.nanos())]
}

/// `span` and 1 ns.
fn over(span: Span) -> Span {
    Span::from_nanos(span.nanos() + 1)
}

/// The first line of `error`.
fn first(error: &str) -> &str {
    error.lines().next().unwrap_or_default()
}

/// The hold of read 0 in [`check`]: half its window of 200 ms, plus [`BOUND`].
const HOLD: Span = Span::from_nanos(100 * Span::MILLISECOND.nanos() + BOUND.nanos());

/// A window over `2 * BOUND`, and its hold.
const WIDE: (Span, Span) = (
    Span::from_nanos(2 * BOUND.nanos() + 2),
    Span::from_nanos(2 * BOUND.nanos() + 1),
);

#[test]
fn a_trace_holds_each_read_to_the_last_step_within_its_limit() {
    let served = create_served();
    let mut stepped = served.clone();
    stepped[6].value = 4.0;
    let mut tick = served.clone();
    tick[3].time = tick[3].time + Span::from_nanos(90 * Span::MILLISECOND.nanos());
    let wide = widened(2, WIDE.0);
    let mut cases = vec![
        (served.clone(), served.clone()),
        (served.clone(), served[..6].to_vec()),
        (stepped.clone(), stepped.clone()),
        (
            tick.clone(),
            later(&tick, Span::from_nanos(6 * Span::MILLISECOND.nanos())),
        ),
    ];
    for by in both(BOUND) {
        cases.push((served.clone(), later(&served, by)));
    }
    for by in both(HOLD) {
        cases.push((served.clone(), moved(&served, 0, by)));
    }
    for by in both(WIDE.1) {
        cases.push((wide.clone(), moved(&wide, 2, by)));
    }
    for (served, stored) in cases {
        let traces = check(&served, &stored).expect("caught up");
        let pairs = (traces.iter())
            .map(|trace| (&trace.served[..], &trace.stored[..], trace.unchanged));
        assert_eq!(pairs.collect::<Vec<_>>(), [(&served[..6], &stored[..6], 3)]);
    }
}

#[test]
fn a_lost_or_extra_sample_fails_the_trace() {
    let served = create_served();
    let not_read = |k, stored: &Sample| {
        format!(
            "plant.spike: sample {k} stored, {stored:?}, is not read {k}, {:?}",
            served[k]
        )
    };
    let extra = Sample {
        time: served[4].time + Span::from_nanos(50 * Span::MILLISECOND.nanos()),
        ..served[4]
    };
    let stale = |k: usize| Sample {
        time: served[k].time,
        ..served[4]
    };
    let cases = [
        (
            served[1..].to_vec(),
            not_read(2, &served[3]),
            "the first read lost",
        ),
        (
            [&served[..1], &served[2..]].concat(),
            not_read(2, &served[3]),
            "a read before the changes lost",
        ),
        (
            [&served[..4], &served[5..]].concat(),
            not_read(4, &served[5]),
            "the last read before the last change lost",
        ),
        (
            [&served[..2], &served[1..]].concat(),
            not_read(3, &served[2]),
            "an extra sample",
        ),
        (
            [&served[..5], &[extra], &served[5..]].concat(),
            not_read(5, &extra),
            "an extra sample before the last step",
        ),
        (
            [&served[..5], &[stale(5), stale(6)]].concat(),
            not_read(5, &stale(5)),
            "the value before the last step in place of the last value",
        ),
    ];
    for (stored, error, case) in cases {
        let failed = check(&served, &stored).unwrap_err();
        assert_eq!(first(&failed), error, "{case}");
    }
    let held = [
        (
            served[..5].to_vec(),
            "plant.spike: InfluxDB holds 5 of the 6 reads",
        ),
        (vec![], "plant.spike: InfluxDB holds 0 of the 6 reads"),
    ];
    for (stored, error) in held {
        assert_eq!(check(&served, &stored).unwrap_err(), error);
    }
}

#[test]
fn a_time_over_its_limit_fails_the_trace() {
    let served = create_served();
    let edge = widened(2, Span::from_nanos(2 * BOUND.nanos()));
    let wide = widened(2, WIDE.0);
    let cases = [
        (&served, 0, over(HOLD)),
        (&served, 1, over(BOUND)),
        (&edge, 2, over(BOUND)),
        (&wide, 2, over(WIDE.1)),
    ];
    for (served, k, limit) in cases {
        for by in both(limit) {
            let stored = moved(served, k, by);
            let failed = check(served, &stored).unwrap_err();
            let error = format!(
                "plant.spike: sample {k} stored, {:?}, is not read {k}, {:?}",
                stored[k], served[k]
            );
            assert_eq!(first(&failed), error, "{by} at {k}");
        }
    }
}

#[test]
fn a_failed_trace_gives_the_offset_and_window_of_each_sample() {
    let served = widened(2, Span::from_nanos(500 * Span::MILLISECOND.nanos()));
    let offset = Span::from_nanos(5 * Span::MILLISECOND.nanos());
    let stored = moved(&later(&served, offset), 1, BOUND);
    assert_eq!(
        check(&served, &stored).unwrap_err(),
        format!(
            "plant.spike: sample 1 stored, {:?}, is not read 1, {:?}\nthe offset of \
             each sample in its window: 0: 5ms in 200ms, 1: 1.005s in 200ms, 2: 5ms in \
             500ms, 3: 5ms in 500ms, 4: 5ms in 200ms, 5: 5ms in 200ms",
            stored[1], served[1]
        )
    );
}

#[test]
fn a_trace_waits_for_the_read_after_the_last_step() {
    let served = &create_served()[..6];
    assert_eq!(
        check(served, served).unwrap_err(),
        "plant.spike: the server answered no read after read 5"
    );
}

#[test]
fn a_far_time_fails_the_trace() {
    let served = create_served();
    let wide = widened(2, WIDE.0);
    let far = |samples: &[Sample], k: usize, nanos| {
        let mut far = samples.to_vec();
        far[k].time = Stamp::from_nanos(nanos);
        far
    };
    let mut wrong = far(&served, 4, i64::MIN);
    wrong[1].value = 9.0;
    let cases = [
        (&served, far(&served, 0, i64::MAX), 0),
        (&wide, far(&wide, 2, i64::MAX), 2),
        (&served, wrong.clone(), 1),
    ];
    for (served, stored, k) in cases {
        let error = format!(
            "plant.spike: sample {k} stored, {:?}, is not read {k}, {:?}",
            stored[k], served[k]
        );
        assert_eq!(first(&check(served, &stored).unwrap_err()), error);
    }
    assert_eq!(
        check(&served, &wrong).unwrap_err(),
        format!(
            "plant.spike: sample 1 stored, {:?}, is not read 1, {:?}\nthe offset of \
             each sample in its window: 0: 0s in 200ms, 1: 0s in 200ms, 2: 0s in \
             200ms, 3: 0s in 200ms, 4: out of range in 200ms, 5: 0s in 200ms",
            wrong[1], served[1]
        )
    );
}

/// [`create_served`], with reads 0 and 1 Good and each read after them Bad. Read 2
/// keeps the value of the reads before it.
fn create_qualities() -> Vec<Sample> {
    let quality = |k| Some(if k < 2 { Quality::GOOD } else { BAD });
    (create_served().into_iter().enumerate())
        .map(|(k, read)| Sample {
            quality: quality(k),
            ..read
        })
        .collect()
}

/// [`traced`] of `stored` against [`create_qualities`], from a node that started at
/// 0.9 s.
fn check_good(stored: &[Sample]) -> Result<(), String> {
    let started = Stamp::from_nanos(900 * Span::MILLISECOND.nanos());
    traced(started, &create_qualities(), stored)
}

#[test]
fn a_sample_that_is_its_good_read_passes() {
    let stored = create_served();
    let mut cases = vec![vec![], stored[..2].to_vec()];
    for by in both(HOLD) {
        cases.push(moved(&stored[..1], 0, by));
    }
    for by in both(BOUND) {
        cases.push(moved(&stored[..2], 1, by));
    }
    for stored in cases {
        assert_eq!(check_good(&stored), Ok(()), "{stored:?}");
    }
}

#[test]
fn a_sample_that_is_not_its_good_read_fails() {
    let stored = create_served();
    let [late, early] = both(over(HOLD));
    let mut wrong = stored[..2].to_vec();
    wrong[1].value = 9.0;
    let cases = [
        (moved(&stored[..1], 0, early), "under the hold of read 0"),
        (moved(&stored[..1], 0, late), "over the hold of read 0"),
        (
            moved(&stored[..2], 1, over(BOUND)),
            "over the target of read 1",
        ),
        (
            moved(&stored[..2], 1, Span::from_nanos(-over(BOUND).nanos())),
            "under the target of read 1",
        ),
        (stored[..3].to_vec(), "read 2, which is Bad"),
        (wrong, "a value of no read"),
    ];
    let served = create_qualities();
    for (stored, case) in cases {
        let k = stored.len() - 1;
        assert_eq!(
            first(&check_good(&stored).unwrap_err()),
            format!(
                "sample {k} stored, {:?}, is not read {k}, {:?}",
                stored[k], served[k]
            ),
            "{case}"
        );
    }
    let far = Sample {
        time: Stamp::from_nanos(i64::MIN),
        ..stored[0]
    };
    assert_eq!(
        check_good(&[far]),
        Err(format!(
            "sample 0 stored, {far:?}, is not read 0, {:?}\nthe offset of each sample \
             in its window: 0: out of range in 200ms",
            served[0]
        ))
    );
}

#[test]
fn a_lost_read_fails_when_the_last_value_came_before() {
    let mut served = create_served();
    served[5].value = 1.0;
    served[6].value = 1.0;
    let stored = [&served[..4], &served[5..]].concat();
    assert_eq!(
        first(&check(&served, &stored).unwrap_err()),
        format!(
            "plant.spike: sample 4 stored, {:?}, is not read 4, {:?}",
            served[5], served[4]
        )
    );
}

#[test]
fn a_trace_with_no_step_panics_with_the_reads_of_the_changes() {
    let served = vec![create_served()[0]; 7];
    let panic = std::panic::catch_unwind(|| check(&served, &served))
        .expect_err("a trace with no step panics");
    let reads = &served[..6];
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some(&*format!(
            "plant.spike: no read of the changes steps the value: {reads:?}"
        ))
    );
}

#[test]
fn each_config_names_the_served_addresses() {
    let opcua = SocketAddr::from(([127, 0, 0, 1], 4840));
    let influx = SocketAddr::from(([127, 0, 0, 1], 9086));
    for hcl in [plant(), QUALITY] {
        let hcl = served(hcl, opcua, influx);
        assert!(hcl.contains("\"opc.tcp://127.0.0.1:4840\""), "{hcl}");
        assert!(hcl.contains("\"http://127.0.0.1:9086\""), "{hcl}");
        assert!(!hcl.contains("localhost"), "{hcl}");
    }
}

#[test]
fn samples_from_an_opc_ua_server_reach_influxdb() {
    let plant = create_plant(&polled(plant()));
    first_stored(&plant, &VARIABLES);
    let reads = change(&plant.opcua, &VARIABLES, Quality::GOOD, TRACED);
    for trace in held(&plant, &reads) {
        let good = |sample: &Sample| sample.quality == Some(Quality::GOOD);
        assert!(trace.served.iter().all(good), "{trace:?}");
    }
}

#[test]
fn a_plant_starts_before_its_first_read() {
    let mut opcua = Opcua::default();
    let mut influx = Influx::default();
    let rig = Rig::new();
    rig.config(&served(plant(), opcua.serve(), influx.serve()));
    let before = opcua.now();
    let plant = Plant::start(rig, opcua, influx);
    plant.rig.apply();
    first_stored(&plant, &VARIABLES[..1]);
    let first = plant.opcua.served()[VARIABLES[0].0][0].time;
    let started = plant.started;
    assert!(
        before <= started && started <= first,
        "the start {started:?} is not between {before:?} and the first read {first:?}"
    );
}

/// `Ok` when `stored` holds the values of `served`, the reads of one variable by a
/// subscription, each value once. Each stored sample is at a time within [`BOUND`] of
/// the first read of its value.
fn subscribed(served: &[Sample], stored: &[Sample]) -> Result<(), String> {
    let mut reads = served.to_vec();
    reads.dedup_by(|b, a| a.value.to_bits() == b.value.to_bits());
    let values =
        |samples: &[Sample]| samples.iter().map(|s| s.value).collect::<Vec<_>>();
    if values(stored) != values(&reads) {
        return Err(format!("{stored:?} are not the values of {reads:?}"));
    }
    for (sample, read) in stored.iter().zip(&reads) {
        let offset = sample.time.checked_since(read.time);
        let near = |offset: Span| offset.nanos().abs() <= BOUND.nanos();
        if !offset.is_some_and(near) {
            return Err(format!("{sample:?} is {offset:?} from its read {read:?}"));
        }
    }
    Ok(())
}

#[test]
fn a_subscribed_plant_stores_each_change_at_its_read() {
    let plant = create_plant(plant());
    first_stored(&plant, &VARIABLES);
    let value = |tick: usize| f64::from(u32::try_from(tick).expect("a few ticks"));
    for tick in 1..=TICKS {
        plant.opcua.change(1, Quality::GOOD);
        // The next change waits until InfluxDB holds this one, so that a stall of the
        // node cannot fill the queue of the server.
        plant.rig.wait("InfluxDB holds the change", || {
            let stored = plant.influx.stored();
            let missed = count(&plant.rig.status(), "plc", "missed");
            VARIABLES.iter().try_for_each(|&(_, channel)| {
                let last = stored[channel].last().map(|sample| sample.value);
                let held = last == Some(value(tick));
                (held.then_some(()))
                    .ok_or(format!("{channel}: {last:?} is last, missed {missed:?}"))
            })
        });
    }
    let (served, stored) = (plant.opcua.served(), plant.influx.stored());
    for (variable, channel) in VARIABLES {
        let checked = subscribed(&served[variable], &stored[channel]);
        checked.unwrap_or_else(|e| panic!("{channel}: {e}"));
    }
    let missed = count(&plant.rig.status(), "plc", "missed");
    assert_eq!(missed, Some(0), "no queue of the server overflows");
}

#[test]
fn a_polled_bad_value_is_left_out_and_counted() {
    let plant = create_plant(&polled(QUALITY));
    let spike = &VARIABLES[..1];
    first_stored(&plant, spike);
    let [reads] = &change(&plant.opcua, spike, BAD, TICKS)[..] else {
        panic!("one variable");
    };
    // One read can fall between the count of the reads and the first change.
    let bad = u64::try_from(reads.changed - reads.unchanged - 1).expect("a few");
    plant.rig.wait("`bad` of `plc` counts the Bad reads", || {
        match count(&plant.rig.status(), "plc", "bad") {
            Some(counted) if counted >= bad => Ok(()),
            counted => Err(format!("{counted:?} of at least {bad}")),
        }
    });
    let mut served = plant.opcua.served();
    let served = served.remove(reads.variable).unwrap_or_default();
    let mut stored = plant.influx.stored();
    let stored = stored.remove(reads.channel).unwrap_or_default();
    assert_eq!(traced(plant.started, &served, &stored), Ok(()));
}

/// A subscription leaves out each change that is Bad or Uncertain, and counts it in
/// `bad` or `uncertain`.
#[test]
fn a_bad_value_is_left_out_and_counted() {
    left_out_and_counted(|plant| plant.opcua.change(TICKS, BAD));
}

/// As [`a_bad_value_is_left_out_and_counted`], with the node stopped for most of the
/// Bad changes, so that the queue of the server drops changes and `missed` counts each
/// overflow, which holds more than one change. The queue holds two round trips of the
/// first read of the server time, so after a slow first read it can drop none.
#[test]
fn a_bad_value_is_counted_after_a_stall() {
    left_out_and_counted(|plant| {
        plant.opcua.change(3, BAD);
        plant.rig.stalled(|| plant.opcua.change(TICKS - 3, BAD));
    });
}

/// The reads of `served` whose value differs from the read before, with `quality`.
fn changes_to(served: &[Sample], quality: Quality) -> u64 {
    let steps = (served.windows(2)).filter(|pair| {
        matches!(pair, [a, b] if a.value.to_bits() != b.value.to_bits()
            && b.quality == Some(quality))
    });
    u64::try_from(steps.count()).expect("a few")
}

/// Changes the value 20 times Bad through `bad`, then 10 times Uncertain and once Good,
/// and asserts the counts of `plc` and the samples that InfluxDB holds.
fn left_out_and_counted(bad: impl FnOnce(&Plant)) {
    let plant = create_plant(QUALITY);
    let [(variable, channel)] = VARIABLES[..1] else {
        panic!("one variable");
    };
    first_stored(&plant, &VARIABLES[..1]);
    // An overflow keeps the newest change, so a wait for a count after each phase holds
    // with a correct kind. Unequal steps, so that a swap of the two counts fails.
    let counted = |key: &str| {
        plant.rig.wait("`plc` counts the phase", || {
            match count(&plant.rig.status(), "plc", key) {
                Some(counted) if counted > 0 => Ok(()),
                counted => Err(format!("{key} {counted:?}")),
            }
        });
    };
    bad(&plant);
    counted("bad");
    plant.opcua.change(TICKS / 2, UNCERTAIN);
    counted("uncertain");
    // A Good last step, so that InfluxDB holds it only after each change before it.
    plant.opcua.change(1, Quality::GOOD);
    let last = f64::from(u32::try_from(TICKS + TICKS / 2 + 1).expect("a few"));
    let (counts, steps) = plant.rig.wait("`plc` counts each change", || {
        let stored = plant.influx.stored().remove(channel).unwrap_or_default();
        let status = plant.rig.status();
        let counts = ["bad", "uncertain", "missed"]
            .map(|key| count(&status, "plc", key).unwrap_or_default());
        let served = &plant.opcua.served()[variable];
        let steps = [BAD, UNCERTAIN].map(|quality| changes_to(served, quality));
        let [bad, uncertain, missed] = counts;
        let held = stored
            .last()
            .is_some_and(|sample| sample.value.to_bits() == last.to_bits());
        if held && (missed > 0 || bad + uncertain >= steps.iter().sum()) {
            Ok((counts, steps))
        } else {
            Err(format!(
                "{counts:?} of the steps {steps:?}, last held: {held}"
            ))
        }
    });
    let [bad, uncertain, missed] = counts;
    let message = format!(
        "bad {bad} and uncertain {uncertain} of the steps {steps:?}, missed {missed}"
    );
    if missed == 0 {
        assert_eq!([bad, uncertain], steps, "{message}");
    } else {
        assert!(bad <= steps[0] && uncertain <= steps[1], "{message}");
    }
    let stored = plant.influx.stored().remove(channel).unwrap_or_default();
    let values: Vec<_> = stored.iter().map(|sample| sample.value).collect();
    assert_eq!(
        values,
        [0.0, last],
        "InfluxDB holds the first and the last value"
    );
}

#[test]
fn a_wrong_config_gives_its_file_line_and_fix() {
    let mut rig = Rig::new();
    let hcl = plant()
        .replacen("data_type", "datatype", 1)
        .replace("\"http://localhost:8086\"", "8086");
    rig.config(&hcl);
    rig.start();
    let output = rig.run(&["plan", "plant.hcl", "--out", "plant.plan"], b"");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(2),
            "",
            "error[document.missing-attribute]: the `channel` block has no `data_type`\n  \
             --> plant.hcl:5:1\n\
             fix: Add a `data_type` attribute such as \"f64\"\n\
             \n\
             error[document.unknown-attribute]: `datatype` is not an attribute of the \
             `channel` block\n  \
             --> plant.hcl:6:3\n\
             fix: Use `kind`, `data_type`, `index`, `quality`, or `unit`, or remove it\n\
             \n\
             error[connector.bad-uri]: a URI is a string, not an integer\n  \
             --> plant.hcl:37:17\n\
             fix: Write an `http` or `https` URI such as \"http://10.0.0.2:8086\"\n\
             \n\
             Run `foundation docs` for the attributes and blocks that each block \
             takes.\n"
        )
    );
    assert!(!rig.dir.join("plant.plan").exists(), "plan writes no plan");
}

#[test]
fn a_wrong_endpoint_shows_in_status_and_heals() {
    let mut opcua = Opcua::default();
    let mut influx = Influx::default();
    let rig = Rig::new();
    let opcua_address = opcua.serve();
    let influx_address = influx.serve();
    influx.stop();
    rig.config(&served(&polled(plant()), opcua_address, influx_address));
    let mut plant = Plant::start(rig, opcua, influx);
    let rig = &plant.rig;
    rig.apply();
    let failing = rig.wait("`influx` shows the failed write", || {
        let status = rig.status();
        status
            .get("influx")
            .filter(|connector| connector.error.is_some())
            .cloned()
            .ok_or_else(|| format!("{status:#?}"))
    });
    assert_eq!(
        failing,
        Connector {
            name: "influx".into(),
            kind: "influx".into(),
            state: "running".into(),
            address: Some(format!("http://{influx_address}")),
            restarts: Some(0),
            counts: Some(BTreeMap::from([
                ("confirmed".into(), 0),
                ("lost".into(), 0),
                ("oversized".into(), 0),
            ])),
            error: Some(Error {
                class: "retry".into(),
                text: format!(
                    "the write to InfluxDB failed: the connect failed: {influx_address} \
                     refused the connection"
                ),
            }),
            note: None,
        }
    );
    rig.wait("`plc` reads the first value of each variable", || {
        let status = rig.status();
        match count(&status, "plc", "in") {
            Some(read) if read >= 3 => Ok(read),
            _ => Err(format!("{status:#?}")),
        }
    });
    let reads = change(&plant.opcua, &VARIABLES, Quality::GOOD, TICKS);
    plant.influx.serve();
    held(&plant, &reads);
}

#[test]
fn a_token_from_the_environment_of_the_node_reaches_influxdb() {
    let plant =
        create_token_plant(|_| "env(\"INFLUX_TOKEN\")".to_owned(), str::to_owned);
    plant.rig.apply();
    first_stored(&plant, &VARIABLES);
}

#[test]
fn a_plan_refuses_a_token_from_a_variable_that_the_node_does_not_set() {
    let plant =
        create_token_plant(|_| "env(\"PLANT_TOKEN\")".to_owned(), str::to_owned);
    refuses_the_token(
        &plant.rig,
        "the secret env(\"PLANT_TOKEN\"): the environment variable PLANT_TOKEN of the \
         node is not set. Start the node with it set to the value",
    );
}

#[test]
fn a_token_file_shows_its_fix_until_it_holds_the_token() {
    for named in [false, true] {
        let mut given = None;
        let source = |rig: &Rig| {
            let (source, path) = if named {
                let path = rig.dir.join("foundation-data/data/secrets/plant.token");
                ("secret(\"plant.token\")".to_owned(), path)
            } else {
                let path = rig.dir.join("token");
                (format!("file({:?})", path.display().to_string()), path)
            };
            given = Some((source.clone(), path));
            source
        };
        let plant = create_token_plant(source, polled);
        let (source, path) = given.expect("the plant names its source");
        let shown = path.display();
        let hidden = if named {
            format!(", unreadable by other users: chmod o-r {shown}")
        } else {
            String::new()
        };
        let missing = format!(
            "the secret {source}: the file {shown} is not there. Write the value to \
             it{hidden}"
        );
        refuses_the_token(&plant.rig, &missing);
        let dir = path.parent().expect("a directory");
        std::fs::create_dir_all(dir).expect("makes the directory");
        std::fs::write(&path, b"t0ken\n").expect("writes the token");
        std::fs::set_permissions(&path, Permissions::from_mode(0o600))
            .expect("makes the token private");
        let output = plant
            .rig
            .run(&["plan", "plant.hcl", "--out", "plant.plan"], b"");
        assert!(output.status.success(), "{}", text(&output.stderr));
        // A run reads the token again, so it shows a file that goes after the plan.
        std::fs::remove_file(&path).expect("removes the token");
        let output = plant.rig.run(&["apply", "plant.plan"], b"");
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert_eq!(
            influx_error(&plant.rig),
            Error {
                class: "retry".into(),
                text: missing,
            }
        );
        std::fs::write(&path, b"t0ken\n").expect("writes the token");
        std::fs::set_permissions(&path, Permissions::from_mode(0o600))
            .expect("makes the token private");
        first_stored(&plant, &VARIABLES);
    }
}

#[test]
fn status_shows_samples_flow() {
    let plant = create_plant(&polled(plant()));
    let rig = &plant.rig;
    // The node reads each variable at each tick, also when its value does not change.
    let before = rig.wait("both counts are above 0", || counts_above(rig, (0, 0)));
    rig.wait("both counts grow", || counts_above(rig, before));
}

/// `browse` of the OPC UA connector gives the points of a folder of the server, with
/// their facts and values.
#[test]
fn browse_gives_the_points_of_the_server_with_their_values() {
    let plant = create_plant(plant());
    let args = [
        "browse",
        "plc",
        "plant.hcl",
        "--start",
        "Plant/Line",
        "--values",
        "--json",
    ];
    let output = plant.rig.run(&args, b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    let found: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json");
    let entries = found["entries"].as_array().expect("a list");
    let temperature = entries
        .iter()
        .find(|entry| entry["path"] == "Plant/Line/Temperature")
        .unwrap_or_else(|| panic!("no Temperature in {found}"));
    let expected = serde_json::json!({
        "path": "Plant/Line/Temperature",
        "tag": "ns=1;s=Plant.Line.Temperature",
        "shape": "point",
        "type": "f64",
        "unit": "°C",
        "unit_code": "CEL",
        "access": "read",
        "value": 21.5,
    });
    assert_eq!(*temperature, expected);
}

/// `import --match` of the OPC UA connector adds a channel for each point that matches,
/// and after `plan` and `apply`, InfluxDB holds samples of each. The match holds only
/// the Doubles `Temperature` and `Speed`, as InfluxDB of the test holds only floats.
/// The import makes the file of mode 0644 a file of mode 0600.
#[test]
fn import_of_a_match_adds_channels_whose_samples_reach_influxdb() {
    let hcl = plant().replacen("\"plant.*\"", "\"plc.*\"", 1);
    assert_ne!(hcl, plant(), "the quickstart config selects `plant.*`");
    let plant = create_plant(&hcl);
    let args = [
        "import",
        "plc",
        "plant.hcl",
        "--start",
        "Plant/Line",
        "--match",
        "*pe*e*",
    ];
    let file = plant.rig.dir.join("plant.hcl");
    std::fs::set_permissions(&file, Permissions::from_mode(0o644))
        .expect("opens the file to others");
    let output = plant.rig.run(&args, b"");
    let expected = "The import added 2 channels to plant.hcl.\n\
                    plc.Temperature Plant/Line/Temperature f64\n\
                    plc.Speed Plant/Line/Speed f64\n";
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (Some(0), expected, "")
    );
    let mode = std::fs::metadata(&file)
        .expect("a file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let output = plant
        .rig
        .run(&["plan", "plant.hcl", "--out", "plant.plan"], b"");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = plant.rig.run(&["apply", "plant.plan"], b"");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let imported = [
        ("ns=1;s=Plant.Line.Temperature", "plc.Temperature"),
        ("ns=1;s=Plant.Line.Speed", "plc.Speed"),
    ];
    first_stored(&plant, &imported);
}

/// An OPC UA connector with only its address and rate.
const BARE: &str = "connector \"plc\" {\n  kind    = \"opcua\"\n  \
                    node    = \"edge\"\n  address = \"opc.tcp://localhost:50000\"\n  \
                    rate    = \"10Hz\"\n}\n";

/// The note of an OPC UA connector with no `read` block.
const NO_READ: &str = "the connector has no `read` block, so it reads nothing. \
                       `browse` lists the points of the server, and `import` adds a \
                       `read` block for each.";

/// The paths that a browse of `Plant/Line` gives.
const LINE: [&str; 7] = [
    "Plant/Line/Plant",
    "Plant/Line/Temperature",
    "Plant/Line/Speed",
    "Plant/Line/Mode",
    "Plant/Line/Profile",
    "Plant/Line/Locked",
    "Plant/Line/Recipe",
];

/// `Ok` when `plc` in `status` runs with no error and shows `note`, and when it has
/// read a sample if `read`.
fn shows(
    status: &BTreeMap<String, Connector>,
    note: Option<&str>,
    read: bool,
) -> Result<(), String> {
    let shown = status.get("plc").is_some_and(|plc| {
        plc.state == "running" && plc.error.is_none() && plc.note.as_deref() == note
    });
    let reads = count(status, "plc", "in").is_some_and(|count| count > 0);
    if shown && reads == read {
        Ok(())
    } else {
        Err(format!("{status:#?}"))
    }
}

/// The path of each entry of the browse output `found`.
fn paths(found: &serde_json::Value) -> Vec<&str> {
    let entries = found["entries"].as_array().expect("a list");
    entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("a path"))
        .collect()
}

/// A config whose OPC UA connector has only an address and a rate plans and applies,
/// and the connector notes that `browse` and `import` add reads. `browse` lists the
/// server, `import` adds two reads, and after a new apply the connector reads them.
#[test]
fn a_connector_with_no_read_applies_and_reads_after_an_import() {
    let plant = create_plant(BARE);
    let rig = &plant.rig;
    rig.wait("plc notes that it reads nothing", || {
        shows(&rig.status(), Some(NO_READ), false)
    });
    let args = [
        "browse",
        "plc",
        "plant.hcl",
        "--start",
        "Plant/Line",
        "--json",
    ];
    let output = rig.run(&args, b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    let found: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(paths(&found), LINE);
    let args = [
        "import",
        "plc",
        "plant.hcl",
        "--start",
        "Plant/Line",
        "--match",
        "*pe*e*",
    ];
    let output = rig.run(&args, b"");
    let expected = "The import added 2 channels to plant.hcl.\n\
                    plc.Temperature Plant/Line/Temperature f64\n\
                    plc.Speed Plant/Line/Speed f64\n\
                    It added the index channel plc.time.\n";
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (Some(0), expected, "")
    );
    rig.apply();
    rig.wait("plc reads the imported points", || {
        shows(&rig.status(), None, true)
    });
}

/// The result of the tool call `name` with `arguments` through `mcp`, as call `id`.
/// Panics on an error reply and on a result that is an error.
fn tool(
    mcp: &mut crate::rig::Talk,
    id: u64,
    name: &str,
    arguments: &serde_json::Value,
) -> serde_json::Value {
    let message = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let reply = mcp.answer(&format!("{message}\n"));
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("json");
    let result = &reply["result"];
    let failed = (&reply["error"], &result["isError"]);
    let none = &serde_json::Value::Null;
    assert_eq!(failed, (none, none), "{name}: {reply}");
    result["structuredContent"].clone()
}

/// [`a_connector_with_no_read_applies_and_reads_after_an_import`] through the tools of
/// `foundation mcp`.
#[test]
fn mcp_applies_a_connector_with_no_read_and_reads_after_an_import() {
    let (mut opcua, mut influx, rig) =
        (Opcua::default(), Influx::default(), Rig::new());
    rig.config(&served(BARE, opcua.serve(), influx.serve()));
    let plant = Plant::start(rig, opcua, influx);
    let rig = &plant.rig;
    let mut mcp = rig.talk(&["mcp"]);
    let mut id = 0;
    let mut call = |name: &str, arguments: serde_json::Value| {
        id += 1;
        tool(&mut mcp, id, name, &arguments)
    };
    let status = |output: serde_json::Value| {
        crate::status::connectors(output.to_string().as_bytes())
    };
    let files = serde_json::json!(["plant.hcl"]);
    let plan = serde_json::json!({ "files": files, "out": "plant.plan" });
    call("plan", plan.clone());
    call("apply", serde_json::json!({ "plan": "plant.plan" }));
    rig.wait("plc notes that it reads nothing", || {
        shows(
            &status(call("status", serde_json::json!({}))),
            Some(NO_READ),
            false,
        )
    });
    let line = serde_json::json!({
        "connector": "plc",
        "files": files,
        "start": "Plant/Line",
    });
    assert_eq!(paths(&call("browse", line.clone())), LINE);
    let mut import = line;
    import["match"] = "*pe*e*".into();
    let imported = call("import", import);
    let channels = imported["channels"].as_array().expect("a list");
    let names: Vec<_> = channels
        .iter()
        .map(|channel| channel["name"].as_str())
        .collect();
    assert_eq!(names, [Some("plc.Temperature"), Some("plc.Speed")]);
    call("plan", plan);
    call("apply", serde_json::json!({ "plan": "plant.plan" }));
    rig.wait("plc reads the imported points", || {
        shows(&status(call("status", serde_json::json!({}))), None, true)
    });
    let output = mcp.close();
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
}

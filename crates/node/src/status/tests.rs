use clock::{Clock, Status};
use estimate::Measurement;
use proptest::prelude::*;
use sim::node::Node;
use types::sample::{Scalar, Type};
use types::time::Span;

use super::{Collector, TABLE, Value};

fn host() -> (sim::Sim, Node) {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    (sim, host)
}

fn measure(host: &Node, offset: Span, error: Span) -> Measurement {
    Measurement::new(host.clock().now(), offset, error).expect("at most 36500 days")
}

fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

fn data_type(value: Value) -> Type {
    match value {
        Value::U8(_) => Type::Scalar(Scalar::U8),
        Value::Span(_) => Type::Scalar(Scalar::Span),
    }
}

/// Asserts that each value has the type of its channel.
fn typed(sample: [Option<Value>; TABLE.len()]) -> [Option<Value>; TABLE.len()] {
    for (channel, value) in TABLE.iter().zip(sample) {
        if let Some(value) = value {
            assert_eq!(data_type(value), channel.data_type, "{}", channel.name);
        }
    }
    sample
}

#[test]
fn lists_the_clock_channels() {
    let names: Vec<&str> = TABLE.iter().map(|c| c.name).collect();
    assert_eq!(names, ["clock.status", "clock.offset", "clock.error"]);
}

#[test]
fn an_unsynced_clock_gives_only_its_status() {
    let (_sim, host) = host();
    let (_clock, reader) = Clock::new(host.clock());
    let collector = Collector::new(reader);
    assert_eq!(typed(collector.collect()), [Some(Value::U8(0)), None, None]);
}

#[test]
fn a_synced_clock_gives_its_offset_and_error() {
    let (_sim, host) = host();
    let (mut clock, reader) = Clock::new(host.clock());
    let source = clock.add();
    clock.push(source, measure(&host, Span::HOUR, ms(2)));
    let collector = Collector::new(reader);
    assert_eq!(
        typed(collector.collect()),
        [
            Some(Value::U8(1)),
            Some(Value::Span(Span::HOUR)),
            Some(Value::Span(ms(2))),
        ]
    );
}

#[test]
fn a_clock_in_holdover_keeps_its_offset() {
    let (_sim, host) = host();
    let (mut clock, reader) = Clock::new(host.clock());
    let source = clock.add();
    clock.push(source, measure(&host, Span::HOUR, ms(2)));
    // One of two sources has no measurement, so no majority agrees.
    clock.add();
    let collector = Collector::new(reader);
    assert_eq!(
        typed(collector.collect()),
        [
            Some(Value::U8(2)),
            Some(Value::Span(Span::HOUR)),
            Some(Value::Span(ms(2))),
        ]
    );
}

#[test]
fn the_error_of_a_clock_in_holdover_grows_by_drift() {
    let (mut sim, host) = host();
    let (mut clock, reader) = Clock::new(host.clock());
    let source = clock.add();
    clock.push(source, measure(&host, Span::HOUR, ms(2)));
    clock.add();
    assert_eq!(sim.run_for(Span::HOUR), Ok(()));
    let Status::Holdover(m, _) = reader.status() else {
        panic!("a clock with no majority holds over");
    };
    assert!(m.error() > ms(2), "{:?}", m.error());
    let [_, _, error] = typed(Collector::new(reader).collect());
    assert_eq!(error, Some(Value::Span(m.error())));
}

proptest! {
    #[test]
    fn gives_the_offset_and_error_that_the_clock_holds(
        offset in any::<i64>(),
        error in 0..=36_500 * Span::DAY.nanos(),
    ) {
        let (_sim, host) = host();
        let (mut clock, reader) = Clock::new(host.clock());
        let source = clock.add();
        let m = measure(&host, Span::from_nanos(offset), Span::from_nanos(error));
        clock.push(source, m);
        let [_, offset, error] = typed(Collector::new(reader).collect());
        prop_assert_eq!(offset, Some(Value::Span(m.offset())));
        prop_assert_eq!(error, Some(Value::Span(m.error())));
    }
}

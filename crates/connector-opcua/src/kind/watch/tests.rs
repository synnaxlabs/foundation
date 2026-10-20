use super::*;

const S: i64 = 1_000_000_000;

fn stamp(nanos: i64) -> Stamp {
    Stamp::from_nanos(nanos)
}

fn change(index: usize, source: Option<i64>, server: Option<i64>) -> Change {
    Change {
        index,
        value: Ok(Value::I32(index.try_into().expect("a small index"))),
        source: source.map(stamp),
        server: server.map(stamp),
        overflow: false,
    }
}

fn reads() -> Vec<Read> {
    ["a", "b"]
        .iter()
        .zip(["ns=1;s=a", "ns=1;s=b"])
        .map(|(channel, node)| Read {
            channel: channel.parse().expect("a name"),
            node: node.parse().expect("a node"),
        })
        .collect()
}

/// A fit of a server clock that reads mesh time exactly.
fn exact() -> Fit {
    let mut fit = Fit::default();
    let at = stamp(1_000 * S);
    let device = device(at).expect("in the map");
    assert_eq!(fit.between(device, at, at), Fitted::Kept);
    fit
}

/// The diagnostic of a value of the read `b` with no timestamp that maps to mesh time.
fn refused() -> Diagnostic {
    Diagnostic::new(
        NO_TIMESTAMP,
        None,
        "the server gives a value of the node `ns=1;s=b` of the channel `b` with no \
         timestamp that maps to mesh time, so the subscription cannot place it"
            .into(),
        "Set `polled = true` to read each node at the rate".into(),
    )
}

#[test]
fn the_map_keeps_each_stamp_from_minus_2_to_the_62_on() {
    let ends = [
        (-(1 << 62), 0),
        (0, 1 << 62),
        (i64::MAX, (1 << 63) + (1 << 62) - 1),
    ];
    for (time, monotonic) in ends {
        assert_eq!(device(stamp(time)), Some(Monotonic(monotonic)), "{time}");
    }
    assert_eq!(device(stamp(-(1 << 62) - 1)), None);
    assert_eq!(device(stamp(i64::MIN)), None);
}

#[test]
fn a_timestamp_of_0_or_before_the_map_is_absent() {
    assert_eq!(timestamp(Some(Stamp::EPOCH)), None);
    assert_eq!(timestamp(Some(stamp(-(1 << 62) - 1))), None);
    assert_eq!(timestamp(None), None);
    assert_eq!(timestamp(Some(stamp(1))), device(stamp(1)));
}

#[test]
fn the_device_time_is_the_source_timestamp_unless_it_is_absent_or_later() {
    let mut drive = Drive::new(2);
    let changes = [
        change(0, Some(5 * S), Some(6 * S)),
        change(0, Some(7 * S), Some(6 * S)),
        change(1, None, Some(6 * S)),
        change(1, Some(0), Some(6 * S)),
        change(1, Some(5 * S), None),
    ];
    let fitted = drive.take(&changes, &mut exact(), stamp(1_000 * S), &reads());
    assert_eq!(fitted.expect("each has a timestamp"), Fitted::Kept);
    let at: Vec<_> = drive.entries.iter().map(|entry| entry.at).collect();
    let device = |seconds| device(stamp(seconds * S)).expect("in the map");
    assert_eq!(at, [5, 6, 6, 6, 5].map(device));
}

#[test]
fn a_change_with_no_timestamp_is_refused() {
    let mut drive = Drive::new(2);
    let changes = [change(0, Some(S), Some(S)), change(1, Some(0), None)];
    let taken = drive.take(&changes, &mut exact(), stamp(1_000 * S), &reads());
    let Err(Error::Config(diagnostics)) = taken else {
        panic!("not refused: {taken:?}");
    };
    assert_eq!(diagnostics, [refused()]);
}

#[test]
fn a_value_takes_the_fitted_time_of_a_timestamp_or_is_refused() {
    let fit = exact();
    let placed = device(stamp(1_000 * S)).expect("in the map");
    let entry = |at, server| Entry {
        index: 1,
        value: Ok(Value::I32(1)),
        at,
        server,
        order: 0,
        held: Stamp::EPOCH,
    };
    let past = Monotonic(u64::MAX);
    let at = fit.mesh(placed).expect("placed");
    let time = |entry: Entry| entry.time(&fit, &reads()).expect("placed");
    assert_eq!(time(entry(placed, Some(past))), at);
    assert_eq!(time(entry(past, Some(placed))), at);
    let refusals = [
        entry(past, Some(past)).time(&fit, &reads()),
        entry(past, None).time(&fit, &reads()),
        entry(placed, Some(placed)).time(&Fit::default(), &reads()),
    ];
    for refusal in refusals {
        let Err(Error::Config(diagnostics)) = refusal else {
            panic!("not refused: {refusal:?}");
        };
        assert_eq!(diagnostics, [refused()]);
    }
}

#[test]
fn a_server_timestamp_after_the_arrival_restarts_the_fit() {
    let mut drive = Drive::new(2);
    let mut fit = exact();
    let late = [change(0, Some(S), Some(2_000 * S))];
    let fitted = drive.take(&late, &mut fit, stamp(1_001 * S), &reads());
    assert_eq!(fitted.expect("has a timestamp"), Fitted::Restarted);
}

#[test]
fn each_overflow_counts() {
    let mut drive = Drive::new(2);
    let mut changes = [change(0, Some(S), Some(S)), change(1, Some(S), Some(S))];
    changes[1].overflow = true;
    let taken = drive.take(&changes, &mut exact(), stamp(1_000 * S), &reads());
    assert_eq!(taken.expect("each has a timestamp"), Fitted::Kept);
    assert_eq!(drive.overflows, 1);
}

#[test]
fn the_order_is_device_time_then_read_then_arrival_with_each_hold() {
    let mut drive = Drive::new(2);
    let changes = [
        change(1, Some(4 * S), Some(9 * S)),
        change(0, Some(4 * S), Some(9 * S)),
        change(0, Some(2 * S), Some(3 * S)),
        change(0, Some(4 * S), Some(8 * S)),
        change(1, None, None),
    ];
    let fit = exact();
    drive.entries.clear();
    for (order, change) in changes.iter().enumerate() {
        drive.entries.push(Entry {
            index: change.index,
            value: change.value,
            at: change
                .source
                .map_or(Monotonic(0), |at| device(at).expect("in it")),
            server: change.server.and_then(device),
            order,
            held: Stamp::EPOCH,
        });
    }
    drive.order(&fit);
    let order: Vec<_> = drive.entries.iter().map(|entry| entry.order).collect();
    assert_eq!(order, [4, 2, 1, 3, 0]);
    let held: Vec<_> = drive.entries.iter().map(|entry| entry.held).collect();
    let mesh = |seconds| fit.mesh(device(stamp(seconds * S)).expect("in it"));
    let mesh = |seconds| mesh(seconds).expect("placed").earliest;
    // The value of read 1 at device time 0 holds until 4 s, and the last of read 1
    // until its server timestamp. Read 0 holds until its next value, or its server
    // timestamp when that is earlier.
    assert_eq!(held, [mesh(4), mesh(3), mesh(4), mesh(8), mesh(9)]);
}

#[test]
fn only_an_unsupported_or_unimplemented_service_is_no_subscription_service() {
    assert!(unsupported(Status::BAD_SERVICE_UNSUPPORTED));
    assert!(unsupported(Status::BAD_NOT_IMPLEMENTED));
    assert!(!unsupported(Status::BAD_NOT_SUPPORTED));
    assert!(!unsupported(Status::GOOD));
}

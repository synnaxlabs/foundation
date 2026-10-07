use super::*;

fn gap() -> Gap {
    named("influx", "edge.time")
}

fn named(connector: &str, index: &str) -> Gap {
    Gap::new(&connector.parse().unwrap(), &index.parse().unwrap())
}

fn written(gap: &mut Gap, stamp: i64) -> String {
    let mut out = b"m v=1 0\n".to_vec();
    gap.line(&mut out, Stamp::from_nanos(stamp));
    String::from_utf8(out).unwrap()
}

#[test]
fn writes_one_line_at_the_stamp_after_the_gap() {
    let mut gap = gap();
    gap.add(10..15);
    assert_eq!(
        written(&mut gap, 1_000),
        "m v=1 0\nfoundation_gaps,connector=influx,index=edge.time count=5i 1000\n"
    );
}

#[test]
fn an_empty_gap_writes_nothing() {
    let mut gap = gap();
    assert_eq!(written(&mut gap, 1_000), "m v=1 0\n");
    gap.add(4..4);
    assert_eq!(written(&mut gap, 1_000), "m v=1 0\n");
}

#[test]
fn a_line_empties_the_gap() {
    let mut gap = gap();
    gap.add(0..5);
    written(&mut gap, 1_000);
    assert_eq!(written(&mut gap, 2_000), "m v=1 0\n");
    gap.add(5..7);
    assert!(written(&mut gap, 3_000).ends_with(" count=2i 3000\n"));
}

#[test]
fn two_gaps_in_a_row_add_their_counts() {
    let mut gap = gap();
    gap.add(0..2);
    gap.add(2..5);
    assert!(written(&mut gap, 1_000).ends_with(" count=5i 1000\n"));
}

#[test]
fn writes_each_name_character_as_is() {
    let mut gap = named("my-influx_1", "@edge.time-a_b");
    gap.add(0..1);
    assert_eq!(
        written(&mut gap, 7),
        "m v=1 0\nfoundation_gaps,connector=my-influx_1,index=@edge.time-a_b count=1i 7\n"
    );
}

#[test]
fn a_reversed_range_adds_nothing() {
    let mut gap = gap();
    let (start, end) = (5, 3);
    gap.add(start..end);
    assert_eq!(written(&mut gap, 1_000), "m v=1 0\n");
}

#[test]
fn holds_a_gap_of_i64_max() {
    let mut gap = gap();
    let max = u64::try_from(i64::MAX).unwrap();
    gap.add(0..max - 1);
    gap.add(max - 1..max);
    assert!(written(&mut gap, 1).ends_with(&format!(" count={}i 1\n", i64::MAX)));
}

#[test]
#[should_panic(expected = "invariant: a gap holds fewer than 2^63 samples, held \
                9223372036854775807, added 1")]
fn panics_past_i64_max() {
    let mut gap = gap();
    let max = u64::try_from(i64::MAX).unwrap();
    gap.add(0..max);
    gap.add(max..max + 1);
}

#[test]
#[should_panic(
    expected = "invariant: a gap holds fewer than 2^63 samples, held 0, added \
                18446744073709551615"
)]
fn panics_on_one_add_past_i64_max() {
    let mut gap = gap();
    gap.add(0..u64::MAX);
}

use super::*;

fn gap() -> Gap {
    named("influx", "edge.time")
}

fn named(connector: &str, index: &str) -> Gap {
    Gap::new(
        &connector.parse().unwrap(),
        &index.parse().unwrap(),
        Path::Live,
    )
}

fn written(gap: &mut Gap, seq: u64, stamp: i64) -> String {
    let mut out = b"m v=1 0\n".to_vec();
    gap.line(&mut out, seq, Stamp::from_nanos(stamp));
    String::from_utf8(out).unwrap()
}

fn count(gap: &mut Gap, seq: u64) -> String {
    written(gap, seq, 1_000)
        .strip_prefix(
            "m v=1 0\nfoundation_gaps,connector=influx,index=edge.time,path=live ",
        )
        .unwrap()
        .into()
}

#[test]
fn writes_one_line_at_the_stamp_after_the_gap() {
    let mut gap = gap();
    gap.add(10..15);
    assert_eq!(
        written(&mut gap, 15, 1_000),
        "m v=1 0\nfoundation_gaps,connector=influx,index=edge.time,path=live \
         count=5i 1000\n"
    );
}

#[test]
fn tags_a_backfill_gap_with_its_path() {
    let mut gap = Gap::new(
        &"influx".parse().unwrap(),
        &"edge.time".parse().unwrap(),
        Path::Backfill,
    );
    gap.add(10..15);
    assert_eq!(
        written(&mut gap, 15, 1_000),
        "m v=1 0\nfoundation_gaps,connector=influx,index=edge.time,path=backfill \
         count=5i 1000\n"
    );
}

#[test]
fn an_empty_gap_writes_nothing() {
    let mut gap = gap();
    assert_eq!(written(&mut gap, 3, 1_000), "m v=1 0\n");
    gap.add(4..4);
    assert_eq!(written(&mut gap, 4, 1_000), "m v=1 0\n");
}

#[test]
fn a_line_empties_the_gap() {
    let mut gap = gap();
    gap.add(0..5);
    written(&mut gap, 5, 1_000);
    assert_eq!(written(&mut gap, 6, 2_000), "m v=1 0\n");
    gap.add(7..9);
    assert_eq!(count(&mut gap, 9), "count=2i 1000\n");
}

#[test]
fn two_gaps_in_a_row_make_one_line() {
    let mut gap = gap();
    gap.add(0..2);
    gap.add(2..5);
    assert_eq!(count(&mut gap, 5), "count=5i 1000\n");
}

#[test]
fn a_gap_reported_again_counts_once() {
    let mut gap = gap();
    gap.add(10..15);
    gap.add(10..15);
    assert_eq!(count(&mut gap, 15), "count=5i 1000\n");
}

#[test]
fn overlapping_gaps_count_each_seq_once() {
    let mut gap = gap();
    gap.add(12..15);
    gap.add(10..13);
    assert_eq!(count(&mut gap, 15), "count=5i 1000\n");
}

#[test]
fn counts_each_seq_up_to_the_next_sample() {
    let mut gap = gap();
    gap.add(10..12);
    gap.add(14..16);
    assert_eq!(count(&mut gap, 18), "count=8i 1000\n");
}

#[test]
fn writes_each_name_character_as_is() {
    let mut gap = named("my-influx_1", "@edge.time-a_b");
    gap.add(0..1);
    assert_eq!(
        written(&mut gap, 1, 7),
        "m v=1 0\nfoundation_gaps,connector=my-influx_1,index=@edge.time-a_b,path=live \
         count=1i 7\n"
    );
}

#[test]
#[should_panic(expected = "invariant: a gap range is not reversed, got 5..3")]
fn panics_on_a_reversed_range() {
    let mut gap = gap();
    let (start, end) = (5, 3);
    gap.add(start..end);
}

#[test]
#[should_panic(expected = "invariant: the sample after the gap 10..15 has seq 14")]
fn panics_on_a_sample_inside_the_gap() {
    let mut gap = gap();
    gap.add(10..15);
    written(&mut gap, 14, 1_000);
}

#[test]
#[should_panic(expected = "invariant: the sample after the gap 10..15 has seq 13")]
fn panics_on_a_sample_inside_a_held_range_after_a_shorter_add() {
    let mut gap = gap();
    gap.add(10..15);
    gap.add(10..12);
    written(&mut gap, 13, 1_000);
}

#[test]
#[should_panic(expected = "invariant: the sample after the gap 10..15 has seq 13")]
fn panics_on_a_sample_inside_a_later_add() {
    let mut gap = gap();
    gap.add(10..12);
    gap.add(13..15);
    written(&mut gap, 13, 1_000);
}

#[test]
fn counts_i64_max_seqs() {
    let max = u64::try_from(i64::MAX).unwrap();
    let mut gap = gap();
    gap.add(1..2);
    assert_eq!(
        count(&mut gap, max + 1),
        format!("count={}i 1000\n", i64::MAX)
    );
}

#[test]
#[should_panic(expected = "invariant: a gap counts fewer than 2^63 seqs, from 0 to \
                9223372036854775808")]
fn panics_on_2_pow_63_seqs() {
    let max = u64::try_from(i64::MAX).unwrap();
    let mut gap = gap();
    gap.add(0..1);
    written(&mut gap, max + 1, 1_000);
}

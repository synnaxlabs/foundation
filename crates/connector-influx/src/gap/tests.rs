use super::*;
use crate::line::Part;

fn written(gap: &mut Gap, stamp: i64) -> String {
    let mut out = b"m v=1 0\n".to_vec();
    gap.line(&mut out, Stamp::from_nanos(stamp));
    String::from_utf8(out).unwrap()
}

#[test]
fn writes_one_line_at_the_stamp_after_the_gap() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(5);
    assert_eq!(
        written(&mut gap, 1_000),
        "m v=1 0\nfoundation_gaps,connector=influx,index=edge.time count=5i 1000\n"
    );
}

#[test]
fn an_empty_gap_writes_nothing() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    assert_eq!(written(&mut gap, 1_000), "m v=1 0\n");
    gap.add(0);
    assert_eq!(written(&mut gap, 1_000), "m v=1 0\n");
}

#[test]
fn a_line_empties_the_gap() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(5);
    written(&mut gap, 1_000);
    assert_eq!(written(&mut gap, 2_000), "m v=1 0\n");
    gap.add(2);
    assert!(written(&mut gap, 3_000).ends_with(" count=2i 3000\n"));
}

#[test]
fn two_gaps_in_a_row_add_their_counts() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(2);
    gap.add(3);
    assert!(written(&mut gap, 1_000).ends_with(" count=5i 1000\n"));
}

#[test]
fn escapes_the_tags() {
    let mut gap = Gap::new("my influx", "a,b=c").unwrap();
    gap.add(1);
    assert_eq!(
        written(&mut gap, 7),
        "m v=1 0\nfoundation_gaps,connector=my\\ influx,index=a\\,b\\=c count=1i 7\n"
    );
}

#[test]
fn refuses_an_empty_connector_or_index() {
    for (connector, index, key) in
        [("", "edge.time", "connector"), ("influx", "", "index")]
    {
        let error = Gap::new(connector, index).unwrap_err();
        assert_eq!(error, Error::Empty(Part::TagValue(key.into())));
        assert_eq!(
            error.to_string(),
            format!("the value of the tag {key:?} is empty")
        );
    }
}

#[test]
fn holds_a_gap_of_i64_max() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(u64::try_from(i64::MAX).unwrap() - 1);
    gap.add(1);
    assert!(written(&mut gap, 1).ends_with(&format!(" count={}i 1\n", i64::MAX)));
}

#[test]
#[should_panic(expected = "invariant: a gap holds fewer than 2^63 samples, held \
                9223372036854775807, added 1")]
fn panics_past_i64_max() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(u64::try_from(i64::MAX).unwrap());
    gap.add(1);
}

#[test]
#[should_panic(
    expected = "invariant: a gap holds fewer than 2^63 samples, held 0, added \
                18446744073709551615"
)]
fn panics_on_one_add_past_i64_max() {
    let mut gap = Gap::new("influx", "edge.time").unwrap();
    gap.add(u64::MAX);
}

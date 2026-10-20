use super::Bytes;

/// `Debug` of `Bytes` gives its length and no pointer.
#[test]
fn debug_gives_only_the_length() {
    let mut byte = 7;
    let bytes = Bytes {
        length: 1,
        data: &raw mut byte,
    };
    assert_eq!(format!("{bytes:?}"), "Bytes { length: 1, .. }");
}

//! Entry points for the fuzz targets. Needs the `sim` feature.

use crate::region::Change;

/// The bytes that the change record in `bytes` encodes to, or `None` when `bytes` is
/// not a change record.
#[must_use]
pub fn round_trip_change(bytes: &[u8]) -> Option<Vec<u8>> {
    let change = Change::decode(bytes).ok()?;
    let mut out = Vec::new();
    change.encode(&mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_gives_its_bytes_and_other_bytes_give_none() {
        let mut home = vec![1, 0x02, 0x01];
        home.extend([0; 14]);
        home.extend([0x0b, 0x0a]);
        home.extend([0; 14]);
        assert_eq!(round_trip_change(&home), Some(home.clone()));
        assert_eq!(round_trip_change(&home[..32]), None);
        assert_eq!(round_trip_change(&[]), None);
    }
}

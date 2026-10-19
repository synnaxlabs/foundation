//! A forward: one hub stream on which a program's node carries the program's write or
//! read to the home of its index. The node sends a [`Forward`], then the program's
//! signed hello and signed first message as the program sent them, then the open body,
//! then the run of the [`places`] of a write.
//!
//! Fields are little-endian.
//!
//! - [`Forward`]: kind 8 (write) or 9 (read), then the count of places (`u32`).
//! - [`places`]: each place (`u32`).

use super::{Error, Fields};

const WRITE: u8 = 8;
const READ: u8 = 9;

/// The first message of a forward. The program's signed hello, its signed first
/// message, the open body, and the places follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forward {
    /// What the program opened.
    pub kind: Kind,
    /// The count of places of the group, at least 1: the places of the run that follows
    /// the open body of a write.
    pub places: u32,
}

impl Forward {
    /// The bytes of a forward.
    pub const LEN: usize = 5;

    /// Writes the forward to `out`.
    ///
    /// # Panics
    ///
    /// When `places` is 0.
    pub fn encode(&self, out: &mut [u8; Self::LEN]) {
        assert!(self.places > 0, "a forward names at least one place");
        let kind = match self.kind {
            Kind::Write => WRITE,
            Kind::Read => READ,
        };
        let [p0, p1, p2, p3] = self.places.to_le_bytes();
        *out = [kind, p0, p1, p2, p3];
    }

    /// The forward in `message`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `message` is empty, [`Error::Kind`] when the first byte
    /// names no forward, [`Error::Length`] when `message` is not [`Forward::LEN`]
    /// bytes, and [`Error::Channels`] when the forward names no place.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(message);
        let Some(kind) = fields.kind() else {
            return Err(Error::Empty);
        };
        let kind = match kind {
            WRITE => Kind::Write,
            READ => Kind::Read,
            kind => return Err(Error::Kind { kind }),
        };
        let places = u32::from_le_bytes(*fields.take()?);
        fields.end()?;
        if places == 0 {
            return Err(Error::Channels);
        }
        Ok(Self { kind, places })
    }
}

/// What the program opened: a write (kind 8) or a read (kind 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A write stream.
    Write,
    /// A read stream.
    Read,
}

/// The places of a forwarded write: each a position (`u32`) in the channels of the
/// open, in ascending order. The run holds exactly [`Forward::places`] places, so the
/// home counts them to find where it ends.
pub mod places {
    use std::slice;

    use super::super::{Error, slots};

    /// The bytes of a place.
    pub const LEN: usize = 4;

    /// Writes `places` to `out`, one message of the run.
    ///
    /// # Panics
    ///
    /// When `out` is empty, or is not 4 bytes for each place.
    pub fn encode(places: impl IntoIterator<Item = u32>, out: &mut [u8]) {
        let slots = slots::<LEN>(out);
        let count = slots.len();
        let mut places = places.into_iter();
        let written = slots
            .iter_mut()
            .zip(places.by_ref())
            .map(|(slot, place)| *slot = place.to_le_bytes())
            .count();
        assert!(
            written == count,
            "out holds {count} places, and places gives {written}"
        );
        assert!(
            places.next().is_none(),
            "out holds {count} places, and places gives more"
        );
    }

    /// The places of the run, in order.
    #[derive(Clone, Debug)]
    pub struct Iter<'r>(slice::Iter<'r, [u8; LEN]>);

    impl Iterator for Iter<'_> {
        type Item = u32;

        fn next(&mut self) -> Option<u32> {
            self.0.next().copied().map(u32::from_le_bytes)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.0.size_hint()
        }
    }

    impl ExactSizeIterator for Iter<'_> {}

    /// The places in `run`, the whole run, for an open of `channels` channels.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `run` is empty, [`Error::Length`] when it is not a whole
    /// count of places, and [`Error::Place`] for the first place that is not after
    /// the place before it or not below `channels`.
    pub fn decode(run: &[u8], channels: u32) -> Result<Iter<'_>, Error> {
        let places = super::super::run::<LEN>(run)?;
        let mut before = None;
        for &bytes in places {
            let place = u32::from_le_bytes(bytes);
            if place >= channels || before.is_some_and(|before| place <= before) {
                return Err(Error::Place { place });
            }
            before = Some(place);
        }
        Ok(Iter(places.iter()))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::hub::tests::{check, check_kinds};

    fn encode(forward: Forward) -> [u8; Forward::LEN] {
        let mut out = [0xaa; Forward::LEN];
        forward.encode(&mut out);
        out
    }

    fn encode_places(places: &[u32]) -> Vec<u8> {
        let len = places.len().checked_mul(places::LEN).expect("the run fits");
        let mut out = vec![0xaa; len];
        places::encode(places.iter().copied(), &mut out);
        out
    }

    fn decode_places(run: &[u8], channels: u32) -> Result<Vec<u32>, Error> {
        places::decode(run, channels).map(Iterator::collect)
    }

    /// The first place of `places` that is not after the place before it or not
    /// below `channels`.
    fn first_bad(places: &[u32], channels: u32) -> Option<u32> {
        let mut before = None;
        places.iter().copied().find(|&place| {
            let bad = place >= channels || before.is_some_and(|before| place <= before);
            before = Some(place);
            bad
        })
    }

    fn kind() -> impl Strategy<Value = Kind> {
        prop_oneof![Just(Kind::Write), Just(Kind::Read)]
    }

    mod forward {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let write = Forward {
                kind: Kind::Write,
                places: 0x0102_0304,
            };
            assert_eq!(encode(write), [8, 4, 3, 2, 1]);
            let read = Forward {
                kind: Kind::Read,
                places: 1,
            };
            assert_eq!(encode(read), [9, 1, 0, 0, 0]);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(Forward::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(Forward::decode, &[8, 9], &[1, 5, 6]);
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(Forward::decode, 8, &[1, 4, 6]);
            check(Forward::decode, 9, &[1, 4, 6]);
        }

        #[test]
        fn refuses_a_forward_of_no_place() {
            assert_eq!(Forward::decode(&[8, 0, 0, 0, 0]), Err(Error::Channels));
            assert_eq!(Forward::decode(&[9, 0, 0, 0, 0]), Err(Error::Channels));
        }

        #[test]
        #[should_panic(expected = "a forward names at least one place")]
        fn panics_on_a_forward_of_no_place() {
            let forward = Forward {
                kind: Kind::Write,
                places: 0,
            };
            forward.encode(&mut [0; Forward::LEN]);
        }

        proptest! {
            #[test]
            fn round_trips(kind in kind(), places in 1..=u32::MAX) {
                let forward = Forward { kind, places };
                prop_assert_eq!(Forward::decode(&encode(forward)), Ok(forward));
            }
        }
    }

    mod run {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            assert_eq!(encode_places(&[1, 0x0102_0304]), [1, 0, 0, 0, 4, 3, 2, 1]);
        }

        #[test]
        fn refuses_an_empty_run() {
            assert_eq!(decode_places(&[], 1), Err(Error::Empty));
        }

        #[test]
        fn refuses_a_run_that_is_not_a_whole_count_of_places() {
            for len in [1, 3, 5, 7] {
                assert_eq!(decode_places(&vec![0; len], 2), Err(Error::Length { len }));
            }
        }

        #[test]
        fn refuses_a_place_at_or_past_the_channels() {
            let run = encode_places(&[0, 2]);
            assert_eq!(decode_places(&run, 2), Err(Error::Place { place: 2 }));
            let run = encode_places(&[u32::MAX]);
            assert_eq!(
                decode_places(&run, u32::MAX),
                Err(Error::Place { place: u32::MAX })
            );
        }

        #[test]
        fn refuses_a_place_that_is_not_after_the_place_before_it() {
            let run = encode_places(&[1, 3, 3]);
            assert_eq!(decode_places(&run, 4), Err(Error::Place { place: 3 }));
            let run = encode_places(&[2, 1]);
            assert_eq!(decode_places(&run, 4), Err(Error::Place { place: 1 }));
        }

        #[test]
        fn refuses_the_first_bad_place() {
            let run = encode_places(&[2, 1, 9]);
            assert_eq!(decode_places(&run, 4), Err(Error::Place { place: 1 }));
        }

        #[test]
        fn decodes_the_last_place_below_the_channels() {
            let run = encode_places(&[0, u32::MAX - 1]);
            assert_eq!(decode_places(&run, u32::MAX), Ok(vec![0, u32::MAX - 1]));
        }

        #[test]
        fn gives_the_count_of_places() {
            let run = encode_places(&[0, 1, 5]);
            let places = places::decode(&run, 6).expect("the places are valid");
            assert_eq!(places.len(), 3);
        }

        #[test]
        #[should_panic(expected = "out holds 2 places, and places gives 1")]
        fn panics_when_places_gives_fewer_than_out_holds() {
            places::encode([0], &mut [0; 8]);
        }

        #[test]
        #[should_panic(expected = "out holds 1 places, and places gives more")]
        fn panics_when_places_gives_more_than_out_holds() {
            places::encode([0, 1], &mut [0; 4]);
        }

        #[test]
        #[should_panic(expected = "out has 6 bytes, not a whole count of 4-byte items")]
        fn panics_when_out_is_not_a_whole_count_of_places() {
            places::encode([0, 1], &mut [0; 6]);
        }

        #[test]
        #[should_panic(expected = "a message of a run holds at least one item")]
        fn panics_when_out_is_empty() {
            places::encode([], &mut []);
        }

        proptest! {
            #[test]
            fn round_trips(
                places in proptest::collection::btree_set(any::<u32>(), 1..64),
                over in 1..=u32::MAX,
            ) {
                let places: Vec<u32> = places.into_iter().collect();
                let last = *places.last().expect("at least one place");
                let channels = last.saturating_add(over);
                prop_assume!(last < channels);
                let run = encode_places(&places);
                prop_assert_eq!(decode_places(&run, channels), Ok(places));
            }

            #[test]
            fn refuses_the_first_place_that_breaks_the_order(
                places in proptest::collection::vec(0..8_u32, 1..8),
                channels in 0..10_u32,
            ) {
                let expected = match first_bad(&places, channels) {
                    Some(place) => Err(Error::Place { place }),
                    None => Ok(places.clone()),
                };
                prop_assert_eq!(
                    decode_places(&encode_places(&places), channels),
                    expected
                );
            }
        }
    }
}

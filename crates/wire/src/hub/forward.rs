//! A forward: one hub stream on which a program's node carries the program's write to
//! the home of its index. The node sends a [`Forward`], then the program's signed hello
//! and signed first message as the program sent them, then the open body, then the run
//! of [`places`].
//!
//! Fields are little-endian.
//!
//! - [`Forward`]: kind 8, then the count of places (`u32`).
//! - [`places`]: each place (`u32`).

use super::{Error, Fields, Writer};

const KIND: u8 = 8;

/// The first message of a forward. The program's signed hello, its signed first
/// message, the open body, and the places follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forward {
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
    /// When `places` is 0, or `out` is not [`Forward::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(self.places > 0, "a forward names at least one place");
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[KIND]);
        out.put(&self.places.to_le_bytes());
    }

    /// The forward in `message`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `message` is empty, [`Error::Kind`] when the first byte
    /// names no forward, [`Error::Length`] when `message` is not [`Forward::LEN`]
    /// bytes, and [`Error::Unplaced`] when the forward names no place.
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(message);
        let Some(kind) = fields.kind() else {
            return Err(Error::Empty);
        };
        if kind != KIND {
            return Err(Error::Kind { kind });
        }
        let places = u32::from_le_bytes(*fields.take()?);
        fields.end()?;
        if places == 0 {
            return Err(Error::Unplaced);
        }
        Ok(Self { places })
    }
}

/// The places of a forwarded write: each a position (`u32`) in the channels of the
/// open, in ascending order. The run holds exactly [`Forward::places`] places, so
/// [`Run`] counts them to find where it ends.
pub mod places {
    use std::slice;

    use super::super::{Error, run, slots};
    use super::Forward;
    use crate::common::rest_of_run;

    /// The bytes of a place.
    pub const LEN: usize = 4;

    /// Writes the first `out.len() / LEN` places of `places` into `out`, one message of
    /// the run. It takes no more, so a caller that passes `by_ref()` writes the rest
    /// into the next message.
    ///
    /// # Panics
    ///
    /// When `out` is empty or not a whole count of places, or when `places` gives fewer
    /// places than `out` holds.
    pub fn encode(places: impl IntoIterator<Item = u32>, out: &mut [u8]) {
        let slots = slots::<LEN>(out);
        let count = slots.len();
        let written = slots
            .iter_mut()
            .zip(places)
            .map(|(slot, place)| *slot = place.to_le_bytes())
            .count();
        assert!(
            written == count,
            "places gave {written} of the {count} places that out holds"
        );
    }

    /// The places of one message of the run, in order.
    #[derive(Clone, Debug)]
    pub struct Iter<'m>(slice::Iter<'m, [u8; LEN]>);

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

    /// The decoder of the run of places at the home: it takes each message of the run,
    /// in order. Each place is after the place before it and below the channels, so the
    /// run gives at most one place for each channel.
    #[derive(Debug)]
    pub struct Run {
        remain: u32,
        channels: u32,
        before: Option<u32>,
    }

    impl Run {
        /// A decoder of the run of `forward`, for an open of `channels` channels.
        #[must_use]
        pub fn new(forward: Forward, channels: u32) -> Self {
            Self {
                remain: forward.places,
                channels,
                before: None,
            }
        }

        /// The places of the next message of the run, in order.
        ///
        /// # Errors
        ///
        /// [`Error::Empty`] when `message` is empty, [`Error::Length`] when it is not a
        /// whole count of places, [`Error::Run`] when it holds more places than remain
        /// (also after the run ended), and [`Error::Place`] for the first place that is
        /// not after the place before it in the run or not below the channels.
        pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<Iter<'m>, Error> {
            let places = run::<LEN>(message)?;
            let remain = rest_of_run(self.remain, places.len())?;
            let mut before = self.before;
            for &bytes in places {
                let place = u32::from_le_bytes(bytes);
                if place >= self.channels
                    || before.is_some_and(|before| place <= before)
                {
                    return Err(Error::Place { place });
                }
                before = Some(place);
            }
            self.remain = remain;
            self.before = before;
            Ok(Iter(places.iter()))
        }

        /// Each place of the run came.
        #[must_use]
        pub fn ended(&self) -> bool {
            self.remain == 0
        }
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

    /// A decoder of a run of `places` places, for an open of `channels` channels.
    fn decoder(places: u32, channels: u32) -> places::Run {
        places::Run::new(Forward { places }, channels)
    }

    fn decode(run: &mut places::Run, message: &[u8]) -> Result<Vec<u32>, Error> {
        run.decode(message).map(Iterator::collect)
    }

    /// The places of `message`, as the first message of a run that it does not fill.
    fn decode_places(message: &[u8], channels: u32) -> Result<Vec<u32>, Error> {
        decode(&mut decoder(u32::MAX, channels), message)
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

    mod forward {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let forward = Forward {
                places: 0x0102_0304,
            };
            assert_eq!(encode(forward), [8, 4, 3, 2, 1]);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(Forward::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(Forward::decode, &[8], &[1, 5, 6]);
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(Forward::decode, 8, &[1, 4, 6]);
        }

        #[test]
        fn refuses_a_forward_of_no_place() {
            assert_eq!(Forward::decode(&[8, 0, 0, 0, 0]), Err(Error::Unplaced));
        }

        #[test]
        #[should_panic(expected = "a forward names at least one place")]
        fn panics_on_a_forward_of_no_place() {
            let forward = Forward { places: 0 };
            forward.encode(&mut [0; Forward::LEN]);
        }

        #[test]
        #[should_panic(expected = "out has 4 bytes, and the message has 5")]
        fn panics_when_out_is_short() {
            Forward { places: 1 }.encode(&mut [0; 4]);
        }

        #[test]
        #[should_panic(expected = "out has 6 bytes, and the message has 5")]
        fn panics_when_out_is_long() {
            Forward { places: 1 }.encode(&mut [0; 6]);
        }

        proptest! {
            #[test]
            fn round_trips(places in 1..=u32::MAX) {
                let forward = Forward { places };
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
        fn refuses_an_empty_message() {
            assert_eq!(decode_places(&[], 1), Err(Error::Empty));
        }

        #[test]
        fn refuses_a_message_that_is_not_a_whole_count_of_places() {
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
        fn refuses_a_place_that_is_not_after_the_last_place_of_the_message_before() {
            for (second, place) in [(2, 2), (1, 1)] {
                let mut run = decoder(3, 4);
                assert_eq!(decode(&mut run, &encode_places(&[1, 2])), Ok(vec![1, 2]));
                assert_eq!(
                    decode(&mut run, &encode_places(&[second])),
                    Err(Error::Place { place })
                );
            }
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
            let mut run = decoder(3, 6);
            let message = encode_places(&[0, 1, 5]);
            let places = run.decode(&message).expect("valid places");
            assert_eq!(places.len(), 3);
        }

        #[test]
        fn refuses_a_message_with_more_places_than_remain() {
            let mut run = decoder(4, 8);
            assert_eq!(decode(&mut run, &encode_places(&[0, 1])), Ok(vec![0, 1]));
            assert_eq!(
                decode(&mut run, &encode_places(&[2, 3, 4])),
                Err(Error::Run {
                    items: 3,
                    remain: 2
                })
            );
        }

        #[test]
        fn refuses_a_message_after_the_run_ended() {
            let mut run = decoder(1, 4);
            assert_eq!(decode(&mut run, &encode_places(&[0])), Ok(vec![0]));
            assert_eq!(
                decode(&mut run, &encode_places(&[1])),
                Err(Error::Run {
                    items: 1,
                    remain: 0
                })
            );
        }

        #[test]
        fn ends_with_the_last_place() {
            let mut run = decoder(3, 8);
            assert!(!run.ended());
            assert_eq!(decode(&mut run, &encode_places(&[0, 1])), Ok(vec![0, 1]));
            assert!(!run.ended());
            assert_eq!(decode(&mut run, &encode_places(&[5])), Ok(vec![5]));
            assert!(run.ended());
        }

        #[test]
        #[should_panic(expected = "places gave 1 of the 2 places that out holds")]
        fn panics_when_places_gives_fewer_than_out_holds() {
            places::encode([0], &mut [0; 8]);
        }

        #[test]
        fn takes_no_more_places_than_out_holds() {
            let mut places = [0, 1, 2].into_iter();
            let mut out = [0xaa; 8];
            places::encode(places.by_ref(), &mut out);
            assert_eq!(out, [0, 0, 0, 0, 1, 0, 0, 0]);
            assert_eq!(places.next(), Some(2));
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
            fn round_trips_in_messages(
                places in proptest::collection::btree_set(any::<u32>(), 1..64),
                over in 1..=u32::MAX,
                per in 1..8_usize,
            ) {
                let places: Vec<u32> = places.into_iter().collect();
                let last = *places.last().expect("at least one place");
                let channels = last.saturating_add(over);
                prop_assume!(last < channels);
                let count = u32::try_from(places.len()).expect("the count fits");
                let mut run = decoder(count, channels);
                let mut iter = places.iter().copied();
                let mut decoded = Vec::new();
                for chunk in places.chunks(per) {
                    prop_assert!(!run.ended());
                    let mut message = encode_places(chunk);
                    message.fill(0xaa);
                    places::encode(iter.by_ref(), &mut message);
                    let got = decode(&mut run, &message);
                    prop_assert_eq!(got.as_deref(), Ok(chunk));
                    decoded.extend_from_slice(chunk);
                }
                prop_assert!(run.ended());
                prop_assert_eq!(iter.next(), None);
                prop_assert_eq!(decoded, places);
            }

            #[test]
            fn refuses_the_first_place_that_breaks_the_order(
                places in proptest::collection::vec(0..8_u32, 1..8),
                channels in 0..10_u32,
                per in 1..4_usize,
            ) {
                let count = u32::try_from(places.len()).expect("the count fits");
                let mut run = decoder(count, channels);
                let refused = places
                    .chunks(per)
                    .find_map(|chunk| decode(&mut run, &encode_places(chunk)).err());
                let expected =
                    first_bad(&places, channels).map(|place| Error::Place { place });
                prop_assert_eq!(refused, expected);
                prop_assert_eq!(run.ended(), expected.is_none());
            }
        }
    }
}

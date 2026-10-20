//! The control sample of an index: a copy of one handoff on the control channel that
//! the index names.

use std::fmt::Write;
use std::sync::Arc;

use buffer::Entry;
use control::Handoff;
use types::channel::Slot;
use types::frame::key_set::KeySet;
use types::frame::{Draft, Form, Frame, Path};
use types::sample::{Scalar, Type};
use types::time::Stamp;

use crate::{order, stored};

/// The control channel of an index: a key set of one group, the control index and
/// its one `string` channel.
#[derive(Debug)]
pub(crate) struct Target {
    set: Arc<KeySet>,
    /// The entry of the `string` channel in `set`.
    channel: usize,
}

impl Target {
    /// The target of `set`.
    ///
    /// # Panics
    ///
    /// If `set` is not one group with one data channel of type `string`.
    pub(crate) fn new(set: Arc<KeySet>) -> Self {
        let [index] = set.groups() else {
            panic!("a control key set has {} groups, not 1", set.groups().len());
        };
        let entries = set.entries();
        assert!(entries.len() == 2, "a control key set has one data channel");
        let channel = usize::from(*index == 0);
        let data_type = entries[channel].data_type;
        assert!(
            data_type == Type::String,
            "the control channel is of type {data_type}, not string"
        );
        Self { set, channel }
    }

    /// The slot of the control index.
    pub(crate) fn slot(&self) -> Slot {
        self.set.entries()[self.set.groups()[0]].slot
    }

    /// The key set of the control index and its channel.
    pub(crate) fn set(&self) -> &Arc<KeySet> {
        &self.set
    }

    /// The frame of one live sample of `handoff` after `after`, the newest live stamp
    /// of the controlled index, at the seq and stamp of `accepted`, and its entry,
    /// stored at mesh time `stored_at`. Its blocks come from `pool`.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when `pool` has no block for the frame or the entry's header.
    ///
    /// # Panics
    ///
    /// If `accepted` is not of one sample.
    pub(crate) fn sample(
        &self,
        pool: &block::Pool,
        handoff: Handoff<'_>,
        after: Option<Stamp>,
        accepted: &order::Accepted,
        stored_at: Stamp,
    ) -> Result<(Frame, Entry), block::Error> {
        let (Some(stamp), 1) = (accepted.last, accepted.seq.end - accepted.seq.start)
        else {
            panic!("invariant: a control sample is one sample: {accepted:?}");
        };
        let value = value(handoff, after);
        let index = self.set.groups()[0];
        let mut series = [
            (
                index,
                encode(Type::Scalar(Scalar::Stamp), &stamp.nanos().to_le_bytes()),
            ),
            (self.channel, encode(Type::String, &raw(&value))),
        ];
        series.sort_unstable_by_key(|(entry, _)| *entry);
        let lens = series
            .each_ref()
            .map(|(entry, bytes)| (*entry, bytes.len()));
        let mut draft = match Draft::new(pool, &self.set, Form::Encoded, &lens) {
            Ok(draft) => draft,
            Err(types::frame::Error::Pool(error)) => return Err(error),
            Err(error) => panic!("invariant: a control sample makes a frame: {error}"),
        };
        for (entry, bytes) in &series {
            let out = draft.series_mut(*entry).expect("the series is present");
            out.copy_from_slice(bytes);
        }
        draft.set_count(0, 1);
        draft.set_seq(0, accepted.seq.start);
        let frame = draft.freeze(Path::Live);
        let entry = stored::entry(pool, &frame, &self.set, stamp, stored_at)?;
        Ok((frame, entry))
    }
}

/// The JSON object of `handoff` after `after`: `{}` when no writer holds control,
/// else the subject, the node key as a UUID, the authority, and `after` in
/// nanoseconds when the index has a sample. A name and a UUID hold no character that
/// JSON escapes.
fn value(handoff: Handoff<'_>, after: Option<Stamp>) -> String {
    let Some(holder) = handoff.to else {
        return "{}".into();
    };
    let mut value = format!(
        "{{\"subject\":\"{}\",\"via\":\"{}\",\"authority\":{}",
        holder.subject.as_str(),
        holder.via,
        holder.authority.0
    );
    if let Some(after) = after {
        write!(value, ",\"after\":{}", after.nanos()).expect("a string takes text");
    }
    value.push('}');
    value
}

/// The raw series of the one string sample `value`.
fn raw(value: &str) -> Vec<u8> {
    let form = codec::Variable::of(Type::String).expect("a string is variable");
    let samples = [value.as_bytes()];
    let mut out = vec![0; form.len(&samples).expect("a short sample")];
    form.write(&samples, &mut out);
    out
}

/// `raw`, the raw series of one sample of `data_type`, encoded.
fn encode(data_type: Type, raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0; codec::max_len(data_type, raw.len())];
    let len = codec::Encoder::new(data_type)
        .encode(1, raw, &mut out)
        .expect("invariant: a control sample encodes");
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use control::Writer;
    use proptest::prelude::*;
    use types::authority::Authority;
    use types::frame::key_set::Group;
    use types::name::Name;
    use types::node;

    use super::*;
    use crate::common::{create_pool, intern, key};

    const VIA: u128 = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10;
    const LIMITS: order::Limits = order::Limits {
        earliest: Stamp::from_nanos(1),
        ahead: types::time::Span::from_nanos(1),
    };

    fn writer(subject: &str, authority: u8) -> Writer {
        Writer {
            subject: subject.parse().expect("a valid name"),
            authority: Authority(authority),
            via: node::Key::from_u128(VIA),
        }
    }

    /// The key set of the index at slot `index` with a channel of `data_type` at
    /// slot `channel`.
    fn control(index: u32, channel: u32, data_type: Type) -> Arc<KeySet> {
        intern(&[Group {
            index: key(Slot::new(index)),
            data: &[(key(Slot::new(channel)), data_type)],
        }])
    }

    mod value {
        use super::*;

        #[test]
        fn names_the_holder_and_the_stamp_before_it() {
            let holder = writer("plant.pump-1", 7);

            let value =
                value(Handoff { to: Some(&holder) }, Some(Stamp::from_nanos(9)));

            assert_eq!(
                value,
                "{\"subject\":\"plant.pump-1\",\"via\":\
                 \"01020304-0506-0708-090a-0b0c0d0e0f10\",\"authority\":7,\"after\":9}"
            );
        }

        #[test]
        fn has_no_stamp_before_the_first_sample() {
            let holder = writer("a", 255);

            let value = value(Handoff { to: Some(&holder) }, None);

            assert_eq!(
                value,
                "{\"subject\":\"a\",\"via\":\"01020304-0506-0708-090a-0b0c0d0e0f10\",\
                 \"authority\":255}"
            );
        }

        #[test]
        fn is_empty_when_no_writer_holds_control() {
            assert_eq!(
                value(Handoff { to: None }, Some(Stamp::from_nanos(9))),
                "{}"
            );
        }
    }

    mod target {
        use super::*;

        #[test]
        fn finds_the_channel_before_or_after_the_index() {
            for (index, channel) in [(2, 3), (3, 2)] {
                let target = Target::new(control(index, channel, Type::String));

                let entry = &target.set().entries()[target.channel];
                assert_eq!(target.slot(), Slot::new(index));
                assert_eq!(entry.slot, Slot::new(channel));
            }
        }

        #[test]
        #[should_panic(expected = "the control channel is of type f64, not string")]
        fn panics_on_a_channel_of_another_type() {
            Target::new(control(2, 3, Type::Scalar(Scalar::F64)));
        }

        #[test]
        #[should_panic(expected = "a control key set has one data channel")]
        fn panics_on_an_index_with_no_channel() {
            Target::new(intern(&[Group {
                index: key(Slot::new(2)),
                data: &[],
            }]));
        }

        #[test]
        #[should_panic(expected = "a control key set has 2 groups, not 1")]
        fn panics_on_two_indexes() {
            Target::new(intern(&[
                Group {
                    index: key(Slot::new(2)),
                    data: &[(key(Slot::new(3)), Type::String)],
                },
                Group {
                    index: key(Slot::new(4)),
                    data: &[],
                },
            ]));
        }

        #[test]
        fn makes_one_live_sample_at_the_seq_and_stamp() {
            let pool = create_pool(1 << 16);
            let target = Target::new(control(3, 2, Type::String));
            let holder = writer("a", 1);
            let tail = order::Tail {
                stamp: None,
                seq: 6,
            };
            let order = order::Order::new(LIMITS, tail, order::Tail::default());
            let accepted = order.companion(Stamp::from_nanos(40));

            let (frame, entry) = target
                .sample(&pool, Handoff { to: Some(&holder) }, None, &accepted, s(50))
                .expect("the pool has room");

            assert_eq!(frame.path(), Path::Live);
            assert_eq!(
                frame.range(0).map(|range| (range.seq, range.count)),
                Some((6, 1))
            );
            assert_eq!(
                decoded(&frame, 1, Type::Scalar(Scalar::Stamp)),
                40_i64.to_le_bytes()
            );
            let string = decoded(&frame, 0, Type::String);
            let form = codec::Variable::of(Type::String).expect("variable");
            let samples: Vec<_> = form.samples(1, &string).collect();
            assert_eq!(
                samples,
                [value(Handoff { to: Some(&holder) }, None).as_bytes()]
            );
            let samples = entry.samples.expect("an entry of samples");
            assert_eq!(
                (
                    entry.slot,
                    entry.first,
                    samples.len.get(),
                    samples.last,
                    entry.stored_at
                ),
                (Slot::new(3), 6, 1, Some(Stamp::from_nanos(40)), s(50))
            );
        }
    }

    fn s(nanos: i64) -> Stamp {
        Stamp::from_nanos(nanos)
    }

    /// The raw series of `entry` in `frame`, of one sample of `data_type`.
    fn decoded(frame: &Frame, entry: usize, data_type: Type) -> Vec<u8> {
        let bytes = frame.series(entry).expect("the series is present");
        let mut out = Vec::new();
        codec::decode(data_type, 1, bytes, &mut out).expect("decodes");
        out
    }

    proptest! {
        #[test]
        fn writes_json_that_reads_back(
            holder in proptest::option::of((
                "@?[A-Za-z0-9_-]{1,63}(\\.@?[A-Za-z0-9_-]{1,63}){0,3}"
                    .prop_filter("a name", |subject| subject.len() <= Name::MAX_BYTES),
                any::<u8>(),
                any::<u128>(),
            )),
            after in proptest::option::of(any::<i64>()),
        ) {
            let holder = holder.map(|(subject, authority, via)| Writer {
                subject: subject.parse().expect("a valid name"),
                authority: Authority(authority),
                via: node::Key::from_u128(via),
            });
            let after = after.map(Stamp::from_nanos);

            let value = value(Handoff { to: holder.as_ref() }, after);

            let read: serde_json::Value = serde_json::from_str(&value).expect("JSON");
            let expected = match &holder {
                None => serde_json::json!({}),
                Some(holder) => {
                    let mut expected = serde_json::json!({
                        "subject": holder.subject.as_str(),
                        "via": holder.via.to_string(),
                        "authority": holder.authority.0,
                    });
                    if let Some(after) = after {
                        expected["after"] = after.nanos().into();
                    }
                    expected
                }
            };
            prop_assert_eq!(read, expected);
        }
    }
}

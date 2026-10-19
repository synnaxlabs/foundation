use proptest::prelude::*;

use super::*;

fn head(time: Time, form: Form) -> Head {
    Head {
        time,
        form,
        series: 2,
    }
}

fn encode_head(head: Head) -> Vec<u8> {
    let mut out = vec![0xaa; Head::LEN];
    head.encode(&mut out);
    out
}

fn encode_reply(reply: Reply) -> Vec<u8> {
    let mut out = vec![0xaa; reply.encoded_len()];
    reply.encode(&mut out);
    out
}

/// `kind`, then `len - 1` zeros.
fn zeros(kind: u8, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    bytes[0] = kind;
    bytes
}

fn taken(frame: u64, seq: u64, count: u32) -> Reply {
    Reply::Outcome {
        frame,
        group: 0,
        outcome: Outcome::Taken {
            range: frame::Range { seq, count },
        },
    }
}

/// A reply of `kind` for group 0x090a of frame 0x0708, with the range `seq` and
/// `count`.
fn range_bytes(kind: u8, seq: u64, count: u32) -> Vec<u8> {
    let mut bytes = vec![kind];
    bytes.extend(0x0708_u64.to_le_bytes());
    bytes.extend(0x090a_u32.to_le_bytes());
    bytes.extend(seq.to_le_bytes());
    bytes.extend(count.to_le_bytes());
    bytes
}

const REFUSALS: [(Refusal, u8); 7] = [
    (Refusal::Waiting, 1),
    (Refusal::Reserved, 2),
    (Refusal::Expired, 3),
    (Refusal::Order, 4),
    (Refusal::Codec, 5),
    (Refusal::Full, 6),
    (Refusal::Large, 7),
];

const ENDS: [(End, u8); 2] = [(End::Removed, 1), (End::Failed, 2)];

mod pins {
    use super::*;

    #[test]
    fn the_head() {
        let mut head = head(Time::Frame(Path::Live), Form::Raw);
        head.series = 0x0304;
        let mut bytes = vec![7, 0, 0, 0x04, 0x03, 0, 0];
        assert_eq!(encode_head(head), bytes);
        head.time = Time::Frame(Path::Backfill);
        head.form = Form::Encoded;
        bytes[1..3].copy_from_slice(&[1, 1]);
        assert_eq!(encode_head(head), bytes);
        let node = Head {
            time: Time::Node,
            form: Form::Raw,
            series: 3,
        };
        assert_eq!(encode_head(node), [7, 2, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn the_counts() {
        let mut out = [0xaa; 8];
        counts::encode([0x0102, 0x0304], &mut out);
        assert_eq!(out, [0x02, 0x01, 0, 0, 0x04, 0x03, 0, 0]);
    }

    #[test]
    fn the_replies() {
        let range = frame::Range {
            seq: 0x0102,
            count: 0x0304,
        };
        let frame = [0x08, 0x07, 0, 0, 0, 0, 0, 0];
        let group = [0x0a, 0x09, 0, 0];
        let mut bytes = [&[1][..], &frame, &group].concat();
        bytes.extend([0x02, 0x01, 0, 0, 0, 0, 0, 0, 0x04, 0x03, 0, 0]);
        let outcome = |outcome| Reply::Outcome {
            frame: 0x0708,
            group: 0x090a,
            outcome,
        };
        assert_eq!(encode_reply(outcome(Outcome::Taken { range })), bytes);
        bytes[0] = 2;
        assert_eq!(encode_reply(outcome(Outcome::Lost { range })), bytes);
        for (refusal, code) in REFUSALS {
            assert_eq!(
                encode_reply(outcome(Outcome::Refused(refusal))),
                [&[3][..], &frame, &group, &[code]].concat()
            );
        }
        for (end, code) in ENDS {
            assert_eq!(
                encode_reply(outcome(Outcome::Ended(end))),
                [&[5][..], &frame, &group, &[code]].concat()
            );
        }
        let stored = Reply::Stored { frames: 0x0506 };
        assert_eq!(encode_reply(stored), [4, 0x06, 0x05, 0, 0, 0, 0, 0, 0]);
    }
}

mod head {
    use super::*;

    #[test]
    #[should_panic(expected = "a head has no series")]
    fn panics_on_an_encode_with_no_series() {
        let head = head(Time::Frame(Path::Live), Form::Raw);
        encode_head(Head { series: 0, ..head });
    }

    #[test]
    #[should_panic(expected = "a head that the node stamps is in the encoded form")]
    fn panics_on_an_encode_of_an_encoded_node_head() {
        encode_head(head(Time::Node, Form::Encoded));
    }
}

mod encode_counts {
    use super::super::counts::encode;

    #[test]
    #[should_panic(expected = "a group of a frame holds no sample")]
    fn panics_on_an_encode_of_a_count_of_0() {
        encode([1, 0], &mut [0; 8]);
    }

    #[test]
    #[should_panic(expected = "counts gave 1 of the 2 counts that out holds")]
    fn panics_on_an_encode_of_fewer_counts_than_out_holds() {
        encode([1], &mut [0; 8]);
    }

    #[test]
    #[should_panic(expected = "out has 9 bytes, not a whole count of 4-byte items")]
    fn panics_on_an_encode_into_a_part_of_a_count() {
        encode([1, 2, 3], &mut [0; 9]);
    }

    #[test]
    fn takes_no_more_counts_than_out_holds() {
        let mut counts = [1, 2, 3].into_iter();
        encode(counts.by_ref(), &mut [0; 8]);
        assert_eq!(counts.next(), Some(3));
    }
}

mod lead {
    use std::sync::Arc;

    use types::frame::{Frame, Layout};
    use types::sample::{Scalar, Type};

    use super::*;
    use crate::hub::tests::key;

    const LIVE: Time = Time::Frame(Path::Live);

    /// The index of group 0 (place 0), an `I64` of it (place 1), and the index of
    /// group 1 (place 2).
    fn create_set() -> Arc<KeySet> {
        let stamp = Type::Scalar(Scalar::Stamp);
        let channels = [
            (key(1), stamp, 0),
            (key(2), Type::Scalar(Scalar::I64), 0),
            (key(3), stamp, 2),
        ];
        Arc::new(KeySet::ordered(channels).expect("no key repeats"))
    }

    fn create_pool() -> block::Pool {
        let config = block::Config::new(1 << 16).expect("the budget fits");
        let memory = block::Heap::new(config.reservation());
        block::Pool::new(config, memory)
    }

    /// A raw frame on `path` of `first` samples of each series of group 0, and of
    /// `second` samples of group 1, which is absent at 0.
    fn create_frame(path: Path, first: u32, second: u32) -> Frame {
        let set = create_set();
        let bytes =
            |count: u32| usize::try_from(count).expect("a u32 fits").strict_mul(8);
        let mut series = vec![(0, bytes(first)), (1, bytes(first))];
        if second > 0 {
            series.push((2, bytes(second)));
        }
        let layout = Layout::new(&set, &series).expect("a valid layout");
        let mut draft = layout.draft(&create_pool(), Form::Raw).expect("a block");
        draft.set_count(0, first);
        if second > 0 {
            draft.set_count(1, second);
        }
        draft.freeze(path)
    }

    fn encode(lead: Lead<'_>) -> Vec<u8> {
        let mut out = vec![0xaa; lead.bytes()];
        lead.encode(&mut out);
        out
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn holds_the_most_bytes_of_a_lead_of_the_set() {
        assert_eq!(Lead::most(&create_set()), 39);
        let frame = create_frame(Path::Live, 2, 1);
        assert_eq!(Lead::new(LIVE, &frame).bytes(), 39);
    }

    #[test]
    fn encodes_the_head_the_ends_and_the_counts_of_a_frame() {
        let frame = create_frame(Path::Backfill, 2, 1);
        let lead = Lead::new(Time::Frame(Path::Backfill), &frame);
        let want = [
            vec![7, 1, 0, 3, 0, 0, 0],
            words(&[0, 16, 1, 32, 2, 40]),
            words(&[2, 1]),
        ];
        assert_eq!(encode(lead), want.concat());
    }

    #[test]
    fn encodes_no_counts_for_a_frame_that_the_node_stamps() {
        let frame = create_frame(Path::Live, 1, 1);
        let lead = Lead::new(Time::Node, &frame);
        let want = [vec![7, 2, 0, 3, 0, 0, 0], words(&[0, 8, 1, 16, 2, 24])];
        assert_eq!(encode(lead), want.concat());
        assert_eq!(lead.messages(16).last(), Some(23..31));
    }

    #[test]
    fn encodes_the_count_of_group_0_alone_when_group_1_is_absent() {
        let frame = create_frame(Path::Live, 3, 0);
        let want = [
            vec![7, 0, 0, 2, 0, 0, 0],
            words(&[0, 24, 1, 48]),
            words(&[3]),
        ];
        assert_eq!(encode(Lead::new(LIVE, &frame)), want.concat());
    }

    #[test]
    fn gives_whole_ends_and_whole_counts_in_each_message() {
        let frame = create_frame(Path::Live, 2, 1);
        let lead = Lead::new(LIVE, &frame);
        let messages = |bytes_max| lead.messages(bytes_max).collect::<Vec<_>>();
        assert_eq!(messages(8), [0..7, 7..15, 15..23, 23..31, 31..39]);
        assert_eq!(messages(19), [0..7, 7..23, 23..31, 31..39]);
        assert_eq!(messages(1024), [0..7, 7..31, 31..39]);
    }

    #[test]
    #[should_panic(expected = "a message of 7 bytes holds no head or no end")]
    fn panics_on_messages_that_hold_no_end() {
        let frame = create_frame(Path::Live, 1, 1);
        drop(Lead::new(LIVE, &frame).messages(7));
    }

    #[test]
    #[should_panic(expected = "the frame is not on the path of its time")]
    fn panics_on_a_frame_on_another_path_than_its_time() {
        let frame = create_frame(Path::Backfill, 1, 1);
        let _lead = Lead::new(LIVE, &frame);
    }

    #[test]
    #[should_panic(expected = "the frame is not on the path of its time")]
    fn panics_on_a_frame_that_the_node_stamps_on_the_backfill_path() {
        let frame = create_frame(Path::Backfill, 1, 1);
        let _lead = Lead::new(Time::Node, &frame);
    }

    #[test]
    #[should_panic(
        expected = "a frame that the node stamps holds other than one sample of each \
                    series"
    )]
    fn panics_on_a_frame_that_the_node_stamps_of_two_samples() {
        let frame = create_frame(Path::Live, 1, 2);
        let _lead = Lead::new(Time::Node, &frame);
    }

    #[test]
    #[should_panic(expected = "a frame that the node stamps is in the encoded form")]
    fn panics_on_an_encoded_frame_that_the_node_stamps() {
        let set = create_set();
        let layout = Layout::new(&set, &[(0, 8), (1, 8)]).expect("a valid layout");
        let mut draft = layout
            .draft(&create_pool(), Form::Encoded)
            .expect("a block");
        draft.set_count(0, 1);
        let _lead = Lead::new(Time::Node, &draft.freeze(Path::Live));
    }

    #[test]
    #[should_panic(expected = "out is not the bytes of the lead")]
    fn panics_on_an_encode_into_another_length() {
        let frame = create_frame(Path::Live, 1, 1);
        Lead::new(LIVE, &frame).encode(&mut [0; 38]);
    }

    #[test]
    #[should_panic(expected = "a group of a frame holds no sample")]
    fn panics_on_an_encode_of_a_group_of_no_sample() {
        let set = create_set();
        let layout = Layout::new(&set, &[(0, 0), (1, 0)]).expect("a valid layout");
        let draft = layout.draft(&create_pool(), Form::Raw).expect("a block");
        let frame = draft.freeze(Path::Live);
        encode(Lead::new(LIVE, &frame));
    }

    proptest! {
        #[test]
        fn frames_take_each_lead_as_its_frame(
            first in 1..4_u32,
            second in 0..4_u32,
            node in any::<bool>(),
            backfill in any::<bool>(),
            bytes_max in 8..48_usize,
        ) {
            let (time, first, second) = match (node, backfill) {
                (true, _) => (Time::Node, 1, second.min(1)),
                (false, false) => (LIVE, first, second),
                (false, true) => (Time::Frame(Path::Backfill), first, second),
            };
            let path = match time {
                Time::Frame(path) => path,
                Time::Node => Path::Live,
            };
            let sent = create_frame(path, first, second);
            let lead = Lead::new(time, &sent);
            let bytes = encode(lead);
            let pool = create_pool();
            let mut frames = Frames::new(create_set());
            for range in lead.messages(bytes_max) {
                prop_assert!(range.len() <= bytes_max);
                let step = frames.decode(&bytes[range]).expect("a message of the lead");
                prop_assert!(matches!(step, Step::Head(_) | Step::More));
                if let Ok(Due::Draft { layout, form }) = frames.due() {
                    let draft = layout.draft(&pool, form).expect("a block");
                    prop_assert!(matches!(frames.draft(draft), Step::More));
                }
            }
            let body = sent.body();
            let Ok(Step::Frame { draft, time: got }) = frames.decode(&body[..]) else {
                panic!("the body gives no frame");
            };
            prop_assert_eq!(got, time);
            let frame = draft.freeze(path);
            let ranges = |frame: &Frame| frame.ranges().collect::<Vec<_>>();
            let ends = |frame: &Frame| frame.ends().collect::<Vec<_>>();
            prop_assert_eq!(ranges(&frame), ranges(&sent));
            prop_assert_eq!(ends(&frame), ends(&sent));
        }
    }
}

mod reply {
    use super::*;

    /// A reply of `kind` and `code` for group 0x090a of frame 0x0708.
    fn coded(kind: u8, code: u8) -> Vec<u8> {
        let mut bytes = vec![kind];
        bytes.extend(0x0708_u64.to_le_bytes());
        bytes.extend(0x090a_u32.to_le_bytes());
        bytes.push(code);
        bytes
    }

    fn refused(code: u8) -> Vec<u8> {
        coded(3, code)
    }

    fn ended(code: u8) -> Vec<u8> {
        coded(5, code)
    }

    #[test]
    fn refuses_each_kind_that_no_reply_has() {
        for kind in (0..=u8::MAX).filter(|kind| !(1..=5).contains(kind)) {
            assert_eq!(Reply::decode(&zeros(kind, 25)), Err(Error::Kind { kind }));
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Reply::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        let lens = [
            (1, 21),
            (1, 24),
            (1, 26),
            (2, 1),
            (2, 26),
            (3, 1),
            (3, 10),
            (3, 13),
            (3, 15),
            (4, 8),
            (5, 9),
            (5, 13),
            (5, 15),
        ];
        for (kind, len) in lens {
            assert_eq!(
                Reply::decode(&zeros(kind, len)),
                Err(Error::Length { len }),
                "{kind}"
            );
        }
        assert_eq!(Reply::decode(&zeros(4, 10)), Err(Error::Length { len: 10 }));
    }

    #[test]
    fn refuses_a_range_of_no_sample() {
        for kind in [1, 2] {
            assert_eq!(
                Reply::decode(&range_bytes(kind, 9, 0)),
                Err(Error::Count { seq: 9 })
            );
        }
    }

    #[test]
    fn refuses_a_range_that_ends_past_the_highest_seq() {
        for kind in [1, 2] {
            assert_eq!(
                Reply::decode(&range_bytes(kind, u64::MAX - 1, 2)),
                Err(Error::Range {
                    seq: u64::MAX - 1,
                    count: 2
                })
            );
        }
        assert_eq!(
            Reply::decode(&range_bytes(1, u64::MAX - 2, 2)),
            Ok(Reply::Outcome {
                frame: 0x0708,
                group: 0x090a,
                outcome: Outcome::Taken {
                    range: frame::Range {
                        seq: u64::MAX - 2,
                        count: 2
                    }
                },
            })
        );
    }

    #[test]
    fn checks_the_length_before_the_range() {
        let mut long = range_bytes(1, u64::MAX, 0);
        long.push(0);
        assert_eq!(Reply::decode(&long), Err(Error::Length { len: 26 }));
    }

    #[test]
    fn refuses_a_code_that_no_refusal_has() {
        for code in (0..=u8::MAX).filter(|code| !(1..=7).contains(code)) {
            assert_eq!(Reply::decode(&refused(code)), Err(Error::Refusal { code }));
        }
    }

    #[test]
    fn refuses_a_code_that_no_end_has() {
        for code in (0..=u8::MAX).filter(|code| !(1..=2).contains(code)) {
            let want = Error::End { code };
            assert_eq!(Reply::decode(&ended(code)), Err(want));
        }
        assert_eq!(
            Error::End { code: 3 }.to_string(),
            "the ended group has code 3, which no end has"
        );
    }

    #[test]
    fn checks_the_length_before_the_code() {
        for kind in [3, 5] {
            let mut long = coded(kind, 0);
            long.push(0);
            assert_eq!(Reply::decode(&long), Err(Error::Length { len: 15 }));
        }
    }

    fn outcome(outcome: Outcome) -> Reply {
        Reply::Outcome {
            frame: 0x0708,
            group: 0x090a,
            outcome,
        }
    }

    #[test]
    fn decodes_each_refusal() {
        for (refusal, code) in REFUSALS {
            assert_eq!(
                Reply::decode(&refused(code)),
                Ok(outcome(Outcome::Refused(refusal)))
            );
        }
    }

    #[test]
    fn decodes_each_end() {
        for (end, code) in ENDS {
            assert_eq!(
                Reply::decode(&ended(code)),
                Ok(outcome(Outcome::Ended(end)))
            );
        }
    }

    #[test]
    fn outcome_len_max_is_the_len_of_the_longest_outcome() {
        let range = frame::Range { seq: 0, count: 1 };
        let mut outcomes = Vec::new();
        // A new outcome does not compile here until the list holds it.
        for outcome in [
            Outcome::Taken { range },
            Outcome::Lost { range },
            Outcome::Refused(Refusal::Waiting),
            Outcome::Ended(End::Removed),
        ] {
            match outcome {
                Outcome::Taken { .. } | Outcome::Lost { .. } => outcomes.push(outcome),
                Outcome::Refused(_) => {
                    outcomes.extend(Refusal::ALL.map(Outcome::Refused));
                }
                Outcome::Ended(_) => outcomes.extend(End::ALL.map(Outcome::Ended)),
            }
        }
        let lens = outcomes.into_iter().map(|outcome| {
            Reply::Outcome {
                frame: 1,
                group: 0,
                outcome,
            }
            .encoded_len()
        });
        assert_eq!(lens.max(), Some(Outcome::LEN_MAX));
    }

    #[test]
    #[should_panic(
        expected = "a reply's range of 0 samples from seq 4 holds none or ends past \
                    the highest seq"
    )]
    fn panics_on_an_encode_of_a_range_of_no_sample() {
        encode_reply(taken(1, 4, 0));
    }

    #[test]
    #[should_panic(
        expected = "a reply's range of 2 samples from seq 18446744073709551615 holds \
                    none or ends past the highest seq"
    )]
    fn panics_on_an_encode_of_a_range_past_the_highest_seq() {
        let range = frame::Range {
            seq: u64::MAX,
            count: 2,
        };
        encode_reply(Reply::Outcome {
            frame: 1,
            group: 0,
            outcome: Outcome::Lost { range },
        });
    }
}

mod replies {
    use super::*;

    fn lost(frame: u64) -> Reply {
        Reply::Outcome {
            frame,
            group: 0,
            outcome: Outcome::Lost {
                range: frame::Range { seq: 0, count: 1 },
            },
        }
    }

    fn refused(frame: u64) -> Reply {
        Reply::Outcome {
            frame,
            group: 0,
            outcome: Outcome::Refused(Refusal::Full),
        }
    }

    /// An outcome of `group` of `frame`.
    fn of(group: u32, reply: Reply) -> Reply {
        let Reply::Outcome { frame, outcome, .. } = reply else {
            panic!("{reply:?} is no outcome")
        };
        Reply::Outcome {
            frame,
            group,
            outcome,
        }
    }

    fn ended(frame: u64, group: u32) -> Reply {
        Reply::Outcome {
            frame,
            group,
            outcome: Outcome::Ended(End::Removed),
        }
    }

    fn stored(frames: u64) -> Reply {
        Reply::Stored { frames }
    }

    /// `replies` after it takes each of `taken`.
    fn after(mut replies: Replies, taken: &[Reply]) -> Replies {
        for &reply in taken {
            assert_eq!(replies.decode(&encode_reply(reply)), Ok(reply));
        }
        replies
    }

    /// The replies after `frames` frames sent, each of group 0.
    fn sent(frames: u64) -> Replies {
        let mut replies = Replies::new();
        for _ in 0..frames {
            replies.sent([0]);
        }
        replies
    }

    /// Asserts that `replies` refuses `reply` with `want`, and changes no count.
    fn refuses(replies: &Replies, reply: Reply, want: Error) {
        let mut after = replies.clone();
        assert_eq!(after.decode(&encode_reply(reply)), Err(want));
        assert_eq!(&after, replies);
    }

    #[test]
    fn refuses_an_outcome_before_a_frame() {
        let want = Error::Unasked { frame: 1, sent: 0 };
        refuses(&Replies::new(), taken(1, 0, 1), want);
        assert_eq!(
            want.to_string(),
            "the outcome answers frame 1, not one of the 0 frames sent"
        );
    }

    #[test]
    fn refuses_an_outcome_of_a_frame_past_those_sent() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        refuses(&replies, lost(3), Error::Unasked { frame: 3, sent: 2 });
    }

    #[test]
    fn refuses_an_outcome_after_the_outcome_of_each_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1), refused(2)]);
        refuses(&replies, lost(3), Error::Unasked { frame: 3, sent: 2 });
        refuses(&replies, lost(2), Error::Order { frame: 2, next: 3 });
    }

    #[test]
    fn refuses_a_second_outcome_of_a_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        let want = Error::Order { frame: 1, next: 2 };
        refuses(&replies, refused(1), want);
        assert_eq!(
            want.to_string(),
            "the outcome answers frame 1, not frame 2, which the next outcome answers"
        );
    }

    #[test]
    fn refuses_an_outcome_that_skips_a_frame() {
        refuses(&sent(2), lost(2), Error::Order { frame: 2, next: 1 });
    }

    #[test]
    fn refuses_an_outcome_of_frame_0() {
        let replies = after(sent(1), &[taken(1, 0, 1)]);
        refuses(&replies, lost(0), Error::Unasked { frame: 0, sent: 1 });
    }

    #[test]
    fn refuses_a_stored_past_the_outcomes() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        let want = Error::Stored {
            frames: 2,
            bound: 1,
        };
        refuses(&replies, stored(2), want);
        assert_eq!(
            want.to_string(),
            "the stored reply covers 2 frames, more than the 1 that got an outcome"
        );
    }

    #[test]
    fn refuses_a_stored_below_the_last() {
        let replies = after(sent(2), &[taken(1, 0, 1), taken(2, 1, 1), stored(2)]);
        let want = Error::Stored {
            frames: 1,
            bound: 2,
        };
        refuses(&replies, stored(1), want);
        assert_eq!(
            want.to_string(),
            "the stored reply covers 1 frames, fewer than the 2 of the last stored \
             reply"
        );
    }

    #[test]
    fn takes_a_stored_of_the_last_count_again() {
        let replies = after(sent(1), &[taken(1, 0, 1), stored(1), stored(1)]);
        assert!(replies.stored());
    }

    #[test]
    fn takes_a_stored_of_no_frame() {
        assert!(after(Replies::new(), &[stored(0)]).stored());
    }

    #[test]
    fn gives_the_error_of_the_decode_and_changes_no_count() {
        let mut replies = sent(1);
        assert_eq!(replies.decode(&[]), Err(Error::Empty));
        assert_eq!(replies.decode(&[9]), Err(Error::Kind { kind: 9 }));
        assert_eq!(replies, sent(1));
    }

    #[test]
    fn is_answered_once_each_frame_sent_has_an_outcome() {
        let replies = sent(2);
        assert!(!replies.answered());
        let replies = after(replies, &[lost(1)]);
        assert!(!replies.answered());
        let replies = after(replies, &[refused(2)]);
        assert!(replies.answered());
        assert!(replies.stored(), "no frame was taken");
    }

    #[test]
    fn is_stored_once_a_stored_covers_the_last_taken_frame() {
        let replies = after(sent(3), &[taken(1, 0, 1), lost(2), refused(3)]);
        assert!(replies.answered());
        assert!(!replies.stored());
        let mut replies = after(replies, &[stored(1)]);
        assert!(
            replies.stored(),
            "the frames after the last taken one need none"
        );
        replies.sent([0]);
        assert!(!replies.stored(), "a frame waits for its outcome");
    }

    #[test]
    fn is_not_stored_while_a_stored_covers_only_an_earlier_taken_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1), taken(2, 1, 1), stored(1)]);
        assert!(!replies.stored());
        assert!(after(replies, &[stored(2)]).stored());
    }

    #[test]
    fn refuses_an_outcome_of_another_group_than_the_next() {
        let mut replies = Replies::new();
        replies.sent([0, 2]);
        let want = Error::Group {
            frame: 1,
            group: 2,
            next: 0,
        };
        refuses(&replies, of(2, lost(1)), want);
        assert_eq!(
            want.to_string(),
            "the outcome answers group 2 of frame 1, not group 0, which the next \
             outcome answers"
        );
        let replies = after(replies, &[lost(1)]);
        let want = Error::Group {
            frame: 1,
            group: 0,
            next: 2,
        };
        refuses(&replies, refused(1), want);
    }

    #[test]
    fn refuses_an_outcome_of_a_group_that_the_frame_does_not_hold() {
        let mut replies = Replies::new();
        replies.sent([0, 2]);
        let want = Error::Group {
            frame: 1,
            group: 1,
            next: 0,
        };
        refuses(&replies, of(1, lost(1)), want);
        let replies = after(replies, &[lost(1)]);
        let want = Error::Group {
            frame: 1,
            group: 1,
            next: 2,
        };
        refuses(&replies, of(1, lost(1)), want);
    }

    #[test]
    fn answers_a_frame_once_each_group_has_its_outcome() {
        let mut replies = Replies::new();
        replies.sent([0, 1]);
        replies.sent([1]);
        let replies = after(replies, &[taken(1, 0, 1)]);
        assert!(!replies.answered());
        let want = Error::Stored {
            frames: 1,
            bound: 0,
        };
        refuses(&replies, stored(1), want);
        refuses(&replies, of(1, lost(2)), Error::Order { frame: 2, next: 1 });
        let replies = after(replies, &[ended(1, 1), stored(1), ended(2, 1)]);
        assert!(replies.answered());
        assert!(replies.stored());
    }

    #[test]
    fn is_not_stored_while_a_frame_with_a_taken_group_is_not() {
        let mut replies = Replies::new();
        replies.sent([0, 1]);
        let replies = after(replies, &[lost(1), of(1, taken(1, 0, 1))]);
        assert!(replies.answered());
        assert!(!replies.stored());
        assert!(after(replies, &[stored(1)]).stored());
    }

    #[test]
    #[should_panic(expected = "a frame sent holds no group")]
    fn panics_on_a_frame_sent_with_no_group() {
        Replies::new().sent([]);
    }

    /// A step of a write stream at the program.
    #[derive(Clone, Debug)]
    enum Step {
        Sent(Vec<u32>),
        Reply(Reply),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        let group = 0..3_u32;
        let step = prop_oneof![
            prop::collection::btree_set(group.clone(), 1..=3)
                .prop_map(|groups| Step::Sent(groups.into_iter().collect())),
            (0..8_u64, group.clone())
                .prop_map(|(frame, group)| Step::Reply(of(group, taken(frame, 0, 1)))),
            (0..8_u64, group.clone())
                .prop_map(|(frame, group)| Step::Reply(of(group, lost(frame)))),
            (0..8_u64, group.clone())
                .prop_map(|(frame, group)| Step::Reply(of(group, refused(frame)))),
            (0..8_u64, group)
                .prop_map(|(frame, group)| Step::Reply(ended(frame, group))),
            (0..8_u64).prop_map(|frames| Step::Reply(stored(frames))),
        ];
        prop::collection::vec(step, 0..64)
    }

    proptest! {
        /// An outcome is taken only for the oldest group of the oldest frame sent with
        /// an outcome due, at any time of its arrival among the sends.
        #[test]
        fn keeps_the_order_of_the_replies(steps in steps()) {
            let mut replies = Replies::new();
            // Each frame and group sent, and the count of them that got an outcome.
            let (mut due, mut at) = (Vec::new(), 0);
            let (mut sent, mut answered, mut taken, mut last) = (0_u64, 0_u64, 0, 0);
            for step in steps {
                let reply = match step {
                    Step::Sent(groups) => {
                        replies.sent(groups.iter().copied());
                        sent = sent.strict_add(1);
                        due.extend(groups.into_iter().map(|group| (sent, group)));
                        continue;
                    }
                    Step::Reply(reply) => reply,
                };
                let before = replies.clone();
                let got = replies.decode(&encode_reply(reply));
                let (next, next_group) = due.get(at).copied().unwrap_or((sent.strict_add(1), 0));
                let want = match reply {
                    Reply::Outcome { frame, .. } if frame == 0 || frame > sent => {
                        Err(Error::Unasked { frame, sent })
                    }
                    Reply::Outcome { frame, .. } if frame != next => {
                        Err(Error::Order { frame, next })
                    }
                    Reply::Outcome { frame, group, .. } if group != next_group => {
                        Err(Error::Group {
                            frame,
                            group,
                            next: next_group,
                        })
                    }
                    Reply::Outcome { frame, outcome, .. } => {
                        at = at.strict_add(1);
                        if due.get(at).is_none_or(|&(next, _)| next != frame) {
                            answered = frame;
                        }
                        if let Outcome::Taken { .. } = outcome {
                            taken = frame;
                        }
                        Ok(reply)
                    }
                    Reply::Stored { frames } if frames < last => {
                        Err(Error::Stored { frames, bound: last })
                    }
                    Reply::Stored { frames } if frames > answered => {
                        Err(Error::Stored { frames, bound: answered })
                    }
                    Reply::Stored { frames } => {
                        last = frames;
                        Ok(reply)
                    }
                };
                prop_assert_eq!(got, want);
                if got.is_err() {
                    prop_assert_eq!(&replies, &before);
                }
                prop_assert_eq!(replies.answered(), answered == sent);
                prop_assert_eq!(replies.stored(), answered == sent && last >= taken);
            }
        }
    }
}

fn reply() -> impl Strategy<Value = Reply> {
    let range = (1..=u32::MAX).prop_flat_map(|count| {
        (0..=u64::MAX.saturating_sub(u64::from(count)))
            .prop_map(move |seq| frame::Range { seq, count })
    });
    let outcome = prop_oneof![
        range.clone().prop_map(|range| Outcome::Taken { range }),
        range.prop_map(|range| Outcome::Lost { range }),
        (0..REFUSALS.len()).prop_map(|at| Outcome::Refused(REFUSALS[at].0)),
        (0..ENDS.len()).prop_map(|at| Outcome::Ended(ENDS[at].0)),
    ];
    prop_oneof![
        (any::<u64>(), any::<u32>(), outcome).prop_map(|(frame, group, outcome)| {
            Reply::Outcome {
                frame,
                group,
                outcome,
            }
        }),
        any::<u64>().prop_map(|frames| Reply::Stored { frames }),
    ]
}

proptest! {
    #[test]
    fn round_trips_a_reply(reply in reply()) {
        prop_assert_eq!(Reply::decode(&encode_reply(reply)), Ok(reply));
    }
}

mod corpus {
    use super::*;

    /// The bytes of the input `$file` of the fuzz target `wire_hub_client`.
    macro_rules! corpus {
        ($file:literal) => {
            include_bytes!(concat!(
                "../../../../../oracles/fuzz/wire_hub_client/",
                $file
            ))
        };
    }

    /// The result of each reply of the fuzz input `bytes` at a `Replies`, as the fuzz
    /// target gives it: each message of a length byte and its bytes, where a message
    /// whose first byte is 0 counts a frame sent, whose groups are its other bytes, or
    /// group 0 when it has none.
    fn decoded(mut bytes: &[u8]) -> Vec<Result<Reply, Error>> {
        let mut replies = Replies::new();
        let mut got = Vec::new();
        while let Some((&len, rest)) = bytes.split_first() {
            let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
            bytes = rest;
            match message {
                [0] => replies.sent([0]),
                [0, groups @ ..] => replies.sent(groups.iter().map(|&g| u32::from(g))),
                _ => got.push(replies.decode(message)),
            }
        }
        got
    }

    fn outcome(frame: u64, outcome: Outcome) -> Reply {
        of(0, frame, outcome)
    }

    fn of(group: u32, frame: u64, outcome: Outcome) -> Reply {
        Reply::Outcome {
            frame,
            group,
            outcome,
        }
    }

    fn range(seq: u64, count: u32) -> frame::Range {
        frame::Range { seq, count }
    }

    /// The inputs `write_replies` and `write_replies_order` in the form where each
    /// outcome holds its frame and group reach each arm that they reached before.
    #[test]
    fn decodes_each_reply_input_of_the_fuzz_target_with_groups() {
        let unasked = |frame| Err(Error::Unasked { frame, sent: 0 });
        assert_eq!(
            decoded(corpus!("write_replies_grouped")),
            [
                unasked(1),
                unasked(2),
                unasked(3),
                Err(Error::Refusal { code: 8 }),
                Err(Error::Refusal { code: 0 }),
                Err(Error::Stored {
                    frames: 12,
                    bound: 0
                }),
                Err(Error::Count { seq: 7 }),
                Err(Error::Range {
                    seq: u64::MAX,
                    count: 2
                }),
                Err(Error::Length { len: 16 }),
                Err(Error::Length { len: 2 }),
                Err(Error::Kind { kind: 6 }),
            ]
        );
        assert_eq!(
            decoded(corpus!("write_replies_order_grouped")),
            [
                Ok(outcome(1, Outcome::Taken { range: range(0, 1) })),
                Err(Error::Unasked { frame: 2, sent: 1 }),
                Err(Error::Stored {
                    frames: 2,
                    bound: 1
                }),
                Ok(Reply::Stored { frames: 1 }),
                Ok(outcome(2, Outcome::Lost { range: range(1, 1) })),
                Err(Error::Stored {
                    frames: 0,
                    bound: 1
                }),
                Ok(outcome(3, Outcome::Refused(Refusal::Order))),
                Err(Error::Refusal { code: 9 }),
                Ok(Reply::Stored { frames: 1 }),
            ]
        );
    }

    /// The inputs `write_outcome_unasked` and `write_outcome_order` in the form where
    /// each outcome holds its group give each error of an outcome of another frame
    /// than the next.
    #[test]
    fn refuses_each_outcome_of_another_frame_in_the_fuzz_inputs() {
        let taken = |frame| Ok(outcome(frame, Outcome::Taken { range: range(0, 1) }));
        assert_eq!(
            decoded(corpus!("write_outcome_unasked_grouped")),
            [
                Err(Error::Unasked { frame: 1, sent: 0 }),
                Err(Error::Unasked { frame: 0, sent: 1 }),
                Err(Error::Unasked { frame: 2, sent: 1 }),
                taken(1),
                Err(Error::Unasked { frame: 2, sent: 1 }),
            ]
        );
        assert_eq!(
            decoded(corpus!("write_outcome_order_grouped")),
            [
                Err(Error::Order { frame: 2, next: 1 }),
                taken(1),
                Err(Error::Order { frame: 1, next: 2 }),
                Err(Error::Unasked { frame: 3, sent: 2 }),
                Ok(outcome(2, Outcome::Refused(Refusal::Order))),
            ]
        );
    }

    /// The input `write_outcome_group` gives each error of an outcome of another group
    /// than the next, and of an end code that no end has.
    #[test]
    fn refuses_each_outcome_of_another_group_in_the_fuzz_inputs() {
        assert_eq!(
            decoded(corpus!("write_outcome_group")),
            [
                Err(Error::Group {
                    frame: 1,
                    group: 2,
                    next: 0
                }),
                Ok(outcome(1, Outcome::Lost { range: range(0, 1) })),
                Err(Error::Stored {
                    frames: 1,
                    bound: 0
                }),
                Err(Error::Group {
                    frame: 1,
                    group: 0,
                    next: 2
                }),
                Err(Error::End { code: 3 }),
                Ok(of(2, 1, Outcome::Ended(End::Failed))),
                Ok(of(1, 2, Outcome::Taken { range: range(1, 1) })),
                Ok(Reply::Stored { frames: 2 }),
            ]
        );
    }

    /// The outcomes of the inputs of the form where an outcome holds no group are
    /// each refused for their length.
    #[test]
    fn refuses_each_outcome_with_no_group_in_the_fuzz_inputs() {
        let inputs: [&[u8]; 4] = [
            corpus!("write_replies_framed"),
            corpus!("write_replies_order_framed"),
            corpus!("write_outcome_unasked"),
            corpus!("write_outcome_order"),
        ];
        for input in inputs {
            let mut bytes = input;
            while let Some((&len, rest)) = bytes.split_first() {
                let (message, rest) = rest.split_at(usize::from(len));
                bytes = rest;
                if let [1..=3, ..] = message {
                    let want = Err(Error::Length { len: message.len() });
                    assert_eq!(Reply::decode(message), want, "{message:?}");
                }
            }
        }
    }
}

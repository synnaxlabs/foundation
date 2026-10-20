use proptest::prelude::*;

use super::*;

fn head(time: Time, form: Form, count: u32) -> Head {
    Head {
        time,
        form,
        count,
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
        outcome: Outcome::Taken {
            range: frame::Range { seq, count },
        },
    }
}

/// A reply of `kind` for frame 0x0708, with the range `seq` and `count`.
fn range_bytes(kind: u8, seq: u64, count: u32) -> Vec<u8> {
    let mut bytes = vec![kind];
    bytes.extend(0x0708_u64.to_le_bytes());
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

mod pins {
    use super::*;

    #[test]
    fn the_head() {
        let mut head = head(Time::Frame(Path::Live), Form::Raw, 0x0102);
        head.series = 0x0304;
        let mut bytes = vec![7, 0, 0, 0x02, 0x01, 0, 0, 0x04, 0x03, 0, 0];
        assert_eq!(encode_head(head), bytes);
        head.time = Time::Frame(Path::Backfill);
        head.form = Form::Encoded;
        bytes[1..3].copy_from_slice(&[1, 1]);
        assert_eq!(encode_head(head), bytes);
        let node = Head {
            time: Time::Node,
            form: Form::Raw,
            count: 1,
            series: 3,
        };
        assert_eq!(encode_head(node), [7, 2, 0, 1, 0, 0, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn the_replies() {
        let range = frame::Range {
            seq: 0x0102,
            count: 0x0304,
        };
        let mut bytes = vec![1, 0x08, 0x07, 0, 0, 0, 0, 0, 0];
        bytes.extend([0x02, 0x01, 0, 0, 0, 0, 0, 0, 0x04, 0x03, 0, 0]);
        assert_eq!(encode_reply(taken(0x0708, 0x0102, 0x0304)), bytes);
        bytes[0] = 2;
        let lost = Reply::Outcome {
            frame: 0x0708,
            outcome: Outcome::Lost { range },
        };
        assert_eq!(encode_reply(lost), bytes);
        for (refusal, code) in REFUSALS {
            let refused = Reply::Outcome {
                frame: 0x0708,
                outcome: Outcome::Refused(refusal),
            };
            assert_eq!(
                encode_reply(refused),
                [3, 0x08, 0x07, 0, 0, 0, 0, 0, 0, code]
            );
        }
        let stored = Reply::Stored { frames: 0x0506 };
        assert_eq!(encode_reply(stored), [4, 0x06, 0x05, 0, 0, 0, 0, 0, 0]);
    }
}

mod head {
    use super::*;

    #[test]
    #[should_panic(expected = "a head has 0 samples of 2 series, not at least 1 of 1")]
    fn panics_on_an_encode_with_no_samples() {
        encode_head(head(Time::Frame(Path::Live), Form::Raw, 0));
    }

    #[test]
    #[should_panic(expected = "a head has 1 samples of 0 series, not at least 1 of 1")]
    fn panics_on_an_encode_with_no_series() {
        let head = head(Time::Frame(Path::Live), Form::Raw, 1);
        encode_head(Head { series: 0, ..head });
    }

    #[test]
    #[should_panic(
        expected = "a head that the node stamps has 1 samples in the Encoded form"
    )]
    fn panics_on_an_encode_of_an_encoded_node_head() {
        encode_head(head(Time::Node, Form::Encoded, 1));
    }

    #[test]
    #[should_panic(
        expected = "a head that the node stamps has 2 samples in the Raw form"
    )]
    fn panics_on_an_encode_of_a_node_head_of_two_samples() {
        encode_head(head(Time::Node, Form::Raw, 2));
    }
}

mod reply {
    use super::*;

    /// A refusal of `code` for frame 0x0708.
    fn refused(code: u8) -> Vec<u8> {
        let mut bytes = vec![3];
        bytes.extend(0x0708_u64.to_le_bytes());
        bytes.push(code);
        bytes
    }

    #[test]
    fn refuses_each_kind_that_no_reply_has() {
        for kind in (0..=u8::MAX).filter(|kind| !(1..=4).contains(kind)) {
            assert_eq!(Reply::decode(&zeros(kind, 21)), Err(Error::Kind { kind }));
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Reply::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        let lens = [
            (1, 20),
            (1, 22),
            (2, 1),
            (2, 22),
            (3, 1),
            (3, 9),
            (3, 11),
            (4, 8),
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
            Ok(taken(0x0708, u64::MAX - 2, 2))
        );
    }

    #[test]
    fn checks_the_length_before_the_range() {
        let mut long = range_bytes(1, u64::MAX, 0);
        long.push(0);
        assert_eq!(Reply::decode(&long), Err(Error::Length { len: 22 }));
    }

    #[test]
    fn refuses_a_code_that_no_refusal_has() {
        for code in (0..=u8::MAX).filter(|code| !(1..=7).contains(code)) {
            assert_eq!(Reply::decode(&refused(code)), Err(Error::Refusal { code }));
        }
    }

    #[test]
    fn checks_the_length_before_the_code() {
        let mut long = refused(0);
        long.push(0);
        assert_eq!(Reply::decode(&long), Err(Error::Length { len: 11 }));
    }

    #[test]
    fn decodes_each_refusal() {
        for (refusal, code) in REFUSALS {
            assert_eq!(
                Reply::decode(&refused(code)),
                Ok(Reply::Outcome {
                    frame: 0x0708,
                    outcome: Outcome::Refused(refusal),
                })
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
        ] {
            match outcome {
                Outcome::Taken { .. } | Outcome::Lost { .. } => outcomes.push(outcome),
                Outcome::Refused(_) => {
                    outcomes.extend(Refusal::ALL.map(Outcome::Refused));
                }
            }
        }
        let lens = outcomes
            .into_iter()
            .map(|outcome| Reply::Outcome { frame: 1, outcome }.encoded_len());
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
            outcome: Outcome::Lost { range },
        });
    }
}

mod replies {
    use super::*;

    fn lost(frame: u64) -> Reply {
        Reply::Outcome {
            frame,
            outcome: Outcome::Lost {
                range: frame::Range { seq: 0, count: 1 },
            },
        }
    }

    fn refused(frame: u64) -> Reply {
        Reply::Outcome {
            frame,
            outcome: Outcome::Refused(Refusal::Full),
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

    fn sent(frames: u64) -> Replies {
        let mut replies = Replies::new();
        for _ in 0..frames {
            replies.sent();
        }
        replies
    }

    /// Asserts that `replies` refuses `reply` with `want`, and changes no count.
    fn refuses(mut replies: Replies, reply: Reply, want: Error) {
        let before = replies;
        assert_eq!(replies.decode(&encode_reply(reply)), Err(want));
        assert_eq!(replies, before);
    }

    #[test]
    fn refuses_an_outcome_before_a_frame() {
        let want = Error::Unasked { frame: 1, sent: 0 };
        refuses(Replies::new(), taken(1, 0, 1), want);
        assert_eq!(
            want.to_string(),
            "the outcome answers frame 1, not one of the 0 frames sent"
        );
    }

    #[test]
    fn refuses_an_outcome_of_a_frame_past_those_sent() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        refuses(replies, lost(3), Error::Unasked { frame: 3, sent: 2 });
    }

    #[test]
    fn refuses_an_outcome_after_the_outcome_of_each_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1), refused(2)]);
        refuses(replies, lost(3), Error::Unasked { frame: 3, sent: 2 });
        refuses(replies, lost(2), Error::Order { frame: 2, next: 3 });
    }

    #[test]
    fn refuses_a_second_outcome_of_a_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        let want = Error::Order { frame: 1, next: 2 };
        refuses(replies, refused(1), want);
        assert_eq!(
            want.to_string(),
            "the outcome answers frame 1, not frame 2, which the next outcome answers"
        );
    }

    #[test]
    fn refuses_an_outcome_that_skips_a_frame() {
        refuses(sent(2), lost(2), Error::Order { frame: 2, next: 1 });
    }

    #[test]
    fn refuses_an_outcome_of_frame_0() {
        let replies = after(sent(1), &[taken(1, 0, 1)]);
        refuses(replies, lost(0), Error::Unasked { frame: 0, sent: 1 });
    }

    #[test]
    fn refuses_a_stored_past_the_outcomes() {
        let replies = after(sent(2), &[taken(1, 0, 1)]);
        let want = Error::Stored {
            frames: 2,
            bound: 1,
        };
        refuses(replies, stored(2), want);
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
        refuses(replies, stored(1), want);
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
        replies.sent();
        assert!(!replies.stored(), "a frame waits for its outcome");
    }

    #[test]
    fn is_not_stored_while_a_stored_covers_only_an_earlier_taken_frame() {
        let replies = after(sent(2), &[taken(1, 0, 1), taken(2, 1, 1), stored(1)]);
        assert!(!replies.stored());
        assert!(after(replies, &[stored(2)]).stored());
    }

    /// A step of a write stream at the program.
    #[derive(Clone, Debug)]
    enum Step {
        Sent,
        Reply(Reply),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        let step = prop_oneof![
            Just(Step::Sent),
            (0..8_u64).prop_map(|frame| Step::Reply(taken(frame, 0, 1))),
            (0..8_u64).prop_map(|frame| Step::Reply(lost(frame))),
            (0..8_u64).prop_map(|frame| Step::Reply(refused(frame))),
            (0..8_u64).prop_map(|frames| Step::Reply(stored(frames))),
        ];
        prop::collection::vec(step, 0..64)
    }

    proptest! {
        /// An outcome is taken only for the oldest frame sent with no outcome, at any
        /// time of its arrival among the sends.
        #[test]
        fn keeps_the_order_of_the_replies(steps in steps()) {
            let mut replies = Replies::new();
            let (mut sent, mut answered, mut taken, mut last) = (0_u64, 0_u64, 0, 0);
            for step in steps {
                let reply = match step {
                    Step::Sent => {
                        replies.sent();
                        sent = sent.strict_add(1);
                        continue;
                    }
                    Step::Reply(reply) => reply,
                };
                let before = replies;
                let got = replies.decode(&encode_reply(reply));
                let next = answered.strict_add(1);
                let want = match reply {
                    Reply::Outcome { frame, .. } if frame == 0 || frame > sent => {
                        Err(Error::Unasked { frame, sent })
                    }
                    Reply::Outcome { frame, .. } if frame != next => {
                        Err(Error::Order { frame, next })
                    }
                    Reply::Outcome { outcome, .. } => {
                        answered = next;
                        if let Outcome::Taken { .. } = outcome {
                            taken = answered;
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
                    prop_assert_eq!(replies, before);
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
    ];
    prop_oneof![
        (any::<u64>(), outcome)
            .prop_map(|(frame, outcome)| Reply::Outcome { frame, outcome }),
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
    /// whose first byte is 0 counts a frame sent.
    fn decoded(bytes: &[u8]) -> Vec<Result<Reply, Error>> {
        let mut replies = Replies::new();
        let mut got = Vec::new();
        for message in crate::common::messages(bytes) {
            if message.first() == Some(&0) {
                replies.sent();
            } else {
                got.push(replies.decode(message));
            }
        }
        got
    }

    fn outcome(frame: u64, outcome: Outcome) -> Reply {
        Reply::Outcome { frame, outcome }
    }

    fn range(seq: u64, count: u32) -> frame::Range {
        frame::Range { seq, count }
    }

    /// The inputs `write_replies` and `write_replies_order` in the form where each
    /// outcome holds its frame reach each arm that they reached before.
    #[test]
    fn decodes_each_reply_input_of_the_fuzz_target_with_frames() {
        let unasked = |frame| Err(Error::Unasked { frame, sent: 0 });
        assert_eq!(
            decoded(corpus!("write_replies_framed")),
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
                Err(Error::Length { len: 12 }),
                Err(Error::Length { len: 2 }),
                Err(Error::Kind { kind: 5 }),
            ]
        );
        assert_eq!(
            decoded(corpus!("write_replies_order_framed")),
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

    /// The inputs `write_outcome_unasked` and `write_outcome_order` give each error of
    /// an outcome of another frame than the next.
    #[test]
    fn refuses_each_outcome_of_another_frame_in_the_fuzz_inputs() {
        let taken = |frame| Ok(outcome(frame, Outcome::Taken { range: range(0, 1) }));
        assert_eq!(
            decoded(corpus!("write_outcome_unasked")),
            [
                Err(Error::Unasked { frame: 1, sent: 0 }),
                Err(Error::Unasked { frame: 0, sent: 1 }),
                Err(Error::Unasked { frame: 2, sent: 1 }),
                taken(1),
                Err(Error::Unasked { frame: 2, sent: 1 }),
            ]
        );
        assert_eq!(
            decoded(corpus!("write_outcome_order")),
            [
                Err(Error::Order { frame: 2, next: 1 }),
                taken(1),
                Err(Error::Order { frame: 1, next: 2 }),
                Err(Error::Unasked { frame: 3, sent: 2 }),
                Ok(outcome(2, Outcome::Refused(Refusal::Order))),
            ]
        );
    }
}

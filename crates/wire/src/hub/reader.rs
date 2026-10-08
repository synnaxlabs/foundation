use super::{BEHIND, Error, HEAD, Head, Mode, OPENED, Open, Reply, ends, rest_of_run};

/// The decoder at the reader's node: it takes each message from the home, in order,
/// and checks the order and the runs of the session.
#[derive(Debug)]
pub struct Reader {
    places: u32,
    latest: bool,
    next: Next,
}

/// The message the session expects next.
#[derive(Clone, Copy, Debug)]
enum Next {
    Opened,
    Head,
    Ended,
    Ends { remain: u32 },
    Body { end: usize, remain: usize },
}

/// A message from the home, decoded.
#[derive(Clone, Debug)]
pub enum FromHome<'m> {
    /// The session is open at the home.
    Opened,
    /// The head of one frame. The run of its ends follows.
    Head(Head),
    /// One message of the ends run. After the last, [`Reader::body`] is `None` when
    /// the frame has no body.
    Ends {
        /// The ends in the message, each a place and an end.
        ends: ends::Iter<'m>,
        /// The run ends with this message.
        last: bool,
    },
    /// One message of the body. It starts where [`Reader::body`] was before the call.
    Body {
        /// The bytes of the message.
        bytes: &'m [u8],
        /// The body ends with this message.
        last: bool,
    },
    /// The session missed a frame, so the home ended it. No message follows.
    Behind,
}

impl Reader {
    /// A decoder for the session that `open` opens. The session has `open.channels`
    /// places.
    ///
    /// # Panics
    ///
    /// When `open.channels` is 0.
    #[must_use]
    pub fn new(open: &Open) -> Self {
        assert!(open.channels > 0, "an open names at least one channel");
        Self {
            places: open.channels,
            latest: open.mode == Mode::Latest,
            next: Next::Opened,
        }
    }

    /// Decodes the next message from the home.
    ///
    /// # Errors
    ///
    /// After `Behind`, each message gives [`Error::Ended`]. Else three checks, in
    /// order; the error is that of the first check that fails.
    ///
    /// 1. The bytes: the [`Error`] of a message that does not decode as a reply
    ///    (`Opened`, a head, or `Behind`) or, where a run continues, as a message of
    ///    that run. A message of a run has no kind, so a reply byte there is run bytes.
    /// 2. The order of the session, whatever the content of a reply:
    ///    [`Error::Unopened`] for a head or a behind before `Opened` and
    ///    [`Error::Reopen`] for a second `Opened`.
    /// 3. The content against the session: [`Error::Latest`] for a behind in a latest
    ///    session, [`Error::Places`] for a head with more series than places,
    ///    [`Error::Run`] for a message with more ends than remain, and [`Error::Body`]
    ///    for a message longer than the rest of the body.
    ///
    /// The session is then not valid ([`MALFORMED`](crate::header::MALFORMED)), and
    /// the caller stops it.
    pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<FromHome<'m>, Error> {
        let (event, next) = match self.next {
            Next::Opened | Next::Head | Next::Ended => self.reply(message)?,
            Next::Ends { remain } => {
                let ends = ends::decode(message)?;
                let remain = rest_of_run(remain, ends.len())?;
                let next = match (remain, ends.last_end()) {
                    (0, Some(0)) => Next::Head,
                    (0, Some(end)) => {
                        let end = body_len(end);
                        Next::Body { end, remain: end }
                    }
                    _ => Next::Ends { remain },
                };
                let last = remain == 0;
                (FromHome::Ends { ends, last }, next)
            }
            Next::Body { end, remain } => {
                let len = message.len();
                if len == 0 {
                    return Err(Error::Empty);
                }
                let remain =
                    remain.checked_sub(len).ok_or(Error::Body { len, remain })?;
                let next = if remain == 0 {
                    Next::Head
                } else {
                    Next::Body { end, remain }
                };
                let last = remain == 0;
                (
                    FromHome::Body {
                        bytes: message,
                        last,
                    },
                    next,
                )
            }
        };
        self.next = next;
        Ok(event)
    }

    /// Where in the body the next message starts, when the next message is body
    /// bytes. Read it before [`Reader::decode`] takes that message.
    #[must_use]
    pub fn body(&self) -> Option<usize> {
        match self.next {
            Next::Body { end, remain } => Some(start(end, remain)),
            Next::Opened | Next::Head | Next::Ends { .. } | Next::Ended => None,
        }
    }

    fn reply<'m>(&self, message: &[u8]) -> Result<(FromHome<'m>, Next), Error> {
        if let Next::Ended = self.next {
            return Err(Error::Ended);
        }
        match (Reply::decode(message)?, self.next) {
            (Reply::Opened, Next::Opened) => Ok((FromHome::Opened, Next::Head)),
            (Reply::Head(_), Next::Opened) => Err(Error::Unopened { kind: HEAD }),
            (Reply::Behind, Next::Opened) => Err(Error::Unopened { kind: BEHIND }),
            (Reply::Behind, _) if self.latest => Err(Error::Latest { kind: BEHIND }),
            (Reply::Behind, _) => Ok((FromHome::Behind, Next::Ended)),
            (Reply::Opened, _) => Err(Error::Reopen { kind: OPENED }),
            (Reply::Head(head), _) if head.series > self.places => Err(Error::Places {
                series: head.series,
                places: self.places,
            }),
            (Reply::Head(head), _) => Ok((
                FromHome::Head(head),
                Next::Ends {
                    remain: head.series,
                },
            )),
        }
    }
}

fn body_len(end: u32) -> usize {
    usize::try_from(end).expect("invariant: a usize holds a u32")
}

fn start(end: usize, remain: usize) -> usize {
    end.checked_sub(remain)
        .expect("invariant: the rest of the body is no longer than the body")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::frame::{Path, Range};

    use super::*;
    use crate::hub::tests::{cut, encode_ends, encode_reply};

    fn open(channels: u32) -> Open {
        Open {
            mode: Mode::Complete { limit_bytes: 0 },
            channels,
        }
    }

    fn head(series: u32) -> Vec<u8> {
        encode_reply(Reply::Head(Head {
            path: Path::Live,
            range: Range { seq: 4, count: 2 },
            series,
        }))
    }

    /// A reader of `places` places that the home opened.
    fn opened(places: u32) -> Reader {
        let mut reader = Reader::new(&open(places));
        reader.decode(&[OPENED]).expect("the session opens");
        reader
    }

    /// The event for `message`, with the ends collected.
    fn event(reader: &mut Reader, message: &[u8]) -> Result<Event, Error> {
        reader.decode(message).map(|event| match event {
            FromHome::Opened => Event::Opened,
            FromHome::Head(head) => Event::Head(head.series),
            FromHome::Ends { ends, last } => Event::Ends(ends.collect(), last),
            FromHome::Body { bytes, last } => Event::Body(bytes.to_vec(), last),
            FromHome::Behind => Event::Behind,
        })
    }

    #[derive(Debug, PartialEq)]
    enum Event {
        Opened,
        Head(u32),
        Ends(Vec<(u32, u32)>, bool),
        Body(Vec<u8>, bool),
        Behind,
    }

    #[test]
    fn decodes_each_message_of_a_session() {
        let mut reader = Reader::new(&open(3));
        assert_eq!(event(&mut reader, &[OPENED]), Ok(Event::Opened));
        assert_eq!(event(&mut reader, &head(2)), Ok(Event::Head(2)));
        assert_eq!(reader.body(), None);
        let ends = encode_ends(&[(0, 3), (2, 13)]);
        let (first, second) = ends.split_at(ends::LEN);
        assert_eq!(
            event(&mut reader, first),
            Ok(Event::Ends(vec![(0, 3)], false))
        );
        assert_eq!(reader.body(), None);
        assert_eq!(
            event(&mut reader, second),
            Ok(Event::Ends(vec![(2, 13)], true))
        );
        assert_eq!(reader.body(), Some(0));
        assert_eq!(
            event(&mut reader, &[7; 9]),
            Ok(Event::Body(vec![7; 9], false))
        );
        assert_eq!(reader.body(), Some(9));
        assert_eq!(
            event(&mut reader, &[8; 4]),
            Ok(Event::Body(vec![8; 4], true))
        );
        assert_eq!(reader.body(), None);
        assert_eq!(event(&mut reader, &head(1)), Ok(Event::Head(1)));
    }

    #[test]
    fn sends_no_body_when_the_last_end_is_0() {
        let mut reader = opened(2);
        reader.decode(&head(2)).expect("the head decodes");
        let ends = encode_ends(&[(0, 0), (1, 0)]);
        assert_eq!(
            event(&mut reader, &ends),
            Ok(Event::Ends(vec![(0, 0), (1, 0)], true))
        );
        assert_eq!(reader.body(), None);
        assert_eq!(event(&mut reader, &head(1)), Ok(Event::Head(1)));
    }

    #[test]
    fn takes_the_body_length_from_the_last_end() {
        let mut reader = opened(2);
        reader.decode(&head(2)).expect("the head decodes");
        reader
            .decode(&encode_ends(&[(0, 16), (1, 0)]))
            .expect("the ends decode");
        assert_eq!(event(&mut reader, &head(1)), Ok(Event::Head(1)));
    }

    #[test]
    fn refuses_a_head_before_opened_whatever_its_series() {
        let mut reader = Reader::new(&open(1));
        assert_eq!(
            reader.decode(&head(3)).err(),
            Some(Error::Unopened { kind: 2 })
        );
        assert_eq!(event(&mut reader, &[OPENED]), Ok(Event::Opened));
        assert_eq!(
            reader.decode(&head(3)).err(),
            Some(Error::Places {
                series: 3,
                places: 1
            })
        );
    }

    #[test]
    fn refuses_a_behind_before_opened() {
        let mut reader = Reader::new(&open(1));
        assert_eq!(
            reader.decode(&[BEHIND]).err(),
            Some(Error::Unopened { kind: 3 })
        );
        assert_eq!(event(&mut reader, &[OPENED]), Ok(Event::Opened));
    }

    #[test]
    fn refuses_a_behind_in_a_latest_session() {
        let latest = Open {
            mode: Mode::Latest,
            channels: 1,
        };
        assert_eq!(
            Reader::new(&latest).decode(&[BEHIND]).err(),
            Some(Error::Unopened { kind: 3 })
        );
        let mut reader = Reader::new(&latest);
        assert_eq!(event(&mut reader, &[OPENED]), Ok(Event::Opened));
        assert_eq!(
            reader.decode(&[BEHIND]).err(),
            Some(Error::Latest { kind: 3 })
        );
        assert_eq!(event(&mut reader, &head(1)), Ok(Event::Head(1)));
    }

    #[test]
    fn ends_the_session_at_behind() {
        let mut reader = opened(1);
        assert_eq!(event(&mut reader, &head(1)), Ok(Event::Head(1)));
        let ends = encode_ends(&[(0, 2)]);
        assert_eq!(
            event(&mut reader, &ends),
            Ok(Event::Ends(vec![(0, 2)], true))
        );
        assert_eq!(
            event(&mut reader, &[4, 5]),
            Ok(Event::Body(vec![4, 5], true))
        );
        assert_eq!(event(&mut reader, &[BEHIND]), Ok(Event::Behind));
        assert_eq!(reader.body(), None);
        for message in [vec![OPENED], vec![BEHIND], head(1), ends, vec![4], vec![]] {
            assert_eq!(reader.decode(&message).err(), Some(Error::Ended));
        }
    }

    #[test]
    fn reads_a_behind_in_a_run_as_run_bytes() {
        let mut reader = opened(1);
        reader.decode(&head(1)).expect("the head decodes");
        assert_eq!(
            reader.decode(&[BEHIND]).err(),
            Some(Error::Length { len: 1 })
        );
        reader
            .decode(&encode_ends(&[(0, 1)]))
            .expect("the end decodes");
        assert_eq!(
            event(&mut reader, &[BEHIND]),
            Ok(Event::Body(vec![3], true))
        );
    }

    #[test]
    fn refuses_a_second_opened() {
        let mut reader = opened(1);
        assert_eq!(
            reader.decode(&[OPENED]).err(),
            Some(Error::Reopen { kind: 1 })
        );
    }

    #[test]
    fn decodes_the_reply_before_the_order() {
        let mut reader = Reader::new(&open(1));
        assert_eq!(
            reader.decode(&[HEAD, 0]).err(),
            Some(Error::Length { len: 2 })
        );
        let mut reader = opened(1);
        assert_eq!(
            reader.decode(&[OPENED, 0]).err(),
            Some(Error::Length { len: 2 })
        );
    }

    #[test]
    fn refuses_a_head_of_no_series() {
        let mut bytes = head(1);
        bytes[14] = 0;
        assert_eq!(opened(1).decode(&bytes).err(), Some(Error::Series));
    }

    #[test]
    fn refuses_a_head_with_more_series_than_places() {
        assert_eq!(
            opened(3).decode(&head(4)).err(),
            Some(Error::Places {
                series: 4,
                places: 3
            })
        );
        let mut reader = opened(u32::MAX);
        assert_eq!(
            event(&mut reader, &head(u32::MAX)),
            Ok(Event::Head(u32::MAX))
        );
    }

    #[test]
    fn refuses_a_run_message_with_more_ends_than_remain() {
        let mut reader = opened(3);
        reader.decode(&head(3)).expect("the head decodes");
        reader
            .decode(&encode_ends(&[(0, 8)]))
            .expect("the first end decodes");
        let three = encode_ends(&[(1, 16), (2, 24), (3, 32)]);
        assert_eq!(
            reader.decode(&three).err(),
            Some(Error::Run {
                items: 3,
                remain: 2
            })
        );
        assert_eq!(
            event(&mut reader, &three[..16]),
            Ok(Event::Ends(vec![(1, 16), (2, 24)], true))
        );
    }

    #[test]
    fn reads_a_body_message_before_the_ends_run_ends_as_ends() {
        let mut reader = opened(2);
        reader.decode(&head(2)).expect("the head decodes");
        reader
            .decode(&encode_ends(&[(0, 8)]))
            .expect("the first end decodes");
        assert_eq!(reader.decode(&[0; 9]).err(), Some(Error::Length { len: 9 }));
        assert_eq!(
            reader.decode(&[0; 17]).err(),
            Some(Error::Length { len: 17 })
        );
        assert_eq!(
            reader.decode(&[0; 16]).err(),
            Some(Error::Run {
                items: 2,
                remain: 1
            })
        );
    }

    #[test]
    fn reads_a_reply_byte_inside_a_run_as_the_run() {
        let mut reader = opened(1);
        reader.decode(&head(1)).expect("the head decodes");
        assert_eq!(
            reader.decode(&[OPENED]).err(),
            Some(Error::Length { len: 1 })
        );
        reader
            .decode(&encode_ends(&[(0, 2)]))
            .expect("the end decodes");
        assert_eq!(
            event(&mut reader, &[OPENED]),
            Ok(Event::Body(vec![OPENED], false))
        );
        assert_eq!(
            event(&mut reader, &[HEAD]),
            Ok(Event::Body(vec![HEAD], true))
        );
    }

    #[test]
    fn reads_a_whole_head_inside_a_run_as_the_run() {
        let mut reader = opened(1);
        reader.decode(&head(1)).expect("the head decodes");
        assert_eq!(
            reader.decode(&head(2)).err(),
            Some(Error::Length { len: 18 })
        );
        reader
            .decode(&encode_ends(&[(0, 18)]))
            .expect("the end decodes");
        assert_eq!(event(&mut reader, &head(2)), Ok(Event::Body(head(2), true)));
    }

    #[test]
    fn refuses_a_body_longer_than_the_last_end() {
        let mut reader = opened(1);
        reader.decode(&head(1)).expect("the head decodes");
        reader
            .decode(&encode_ends(&[(0, 10)]))
            .expect("the end decodes");
        assert_eq!(
            reader.decode(&[0; 11]).err(),
            Some(Error::Body {
                len: 11,
                remain: 10
            })
        );
        reader.decode(&[0; 6]).expect("the body starts");
        assert_eq!(
            reader.decode(&[0; 5]).err(),
            Some(Error::Body { len: 5, remain: 4 })
        );
        assert_eq!(reader.body(), Some(6));
    }

    #[test]
    fn refuses_an_empty_message_at_each_step() {
        let mut reader = Reader::new(&open(1));
        assert_eq!(reader.decode(&[]).err(), Some(Error::Empty));
        reader.decode(&[OPENED]).expect("the session opens");
        assert_eq!(reader.decode(&[]).err(), Some(Error::Empty));
        reader.decode(&head(1)).expect("the head decodes");
        assert_eq!(reader.decode(&[]).err(), Some(Error::Empty));
        reader
            .decode(&encode_ends(&[(0, 1)]))
            .expect("the end decodes");
        assert_eq!(reader.decode(&[]).err(), Some(Error::Empty));
    }

    /// A frame the home sends: its series count is the count of its ends, and its body
    /// is as long as its last end.
    fn frame(places: u32) -> impl Strategy<Value = (Vec<(u32, u32)>, Vec<u8>)> {
        let series = 1..=usize::try_from(places).expect("the places fit a usize");
        proptest::collection::vec((any::<u32>(), 0..40_u32), series).prop_flat_map(
            |ends| {
                let len = ends.last().map_or(0, |&(_, end)| end);
                let len = usize::try_from(len).expect("the end fits a usize");
                (Just(ends), proptest::collection::vec(any::<u8>(), len))
            },
        )
    }

    proptest! {
        #[test]
        fn decodes_a_session_cut_into_messages_of_any_size(
            (places, frames) in (1..=6_u32).prop_flat_map(|places| {
                (Just(places), proptest::collection::vec(frame(places), 0..4))
            }),
            sizes in proptest::collection::vec(1..=24_usize, 1..8),
            behind in any::<bool>(),
        ) {
            let mut sizes = sizes.into_iter().cycle();
            let mut reader = Reader::new(&open(places));
            prop_assert_eq!(event(&mut reader, &[OPENED]), Ok(Event::Opened));
            for (ends, body) in &frames {
                let series = u32::try_from(ends.len()).expect("the ends fit a u32");
                prop_assert_eq!(event(&mut reader, &head(series)), Ok(Event::Head(series)));
                let mut messages = cut(ends, &mut sizes).into_iter().peekable();
                while let Some(message) = messages.next() {
                    prop_assert_eq!(reader.body(), None);
                    let last = messages.peek().is_none();
                    prop_assert_eq!(
                        event(&mut reader, &encode_ends(message)),
                        Ok(Event::Ends(message.to_vec(), last))
                    );
                }
                let mut messages = cut(body, &mut sizes).into_iter().peekable();
                let mut at = 0_usize;
                while let Some(message) = messages.next() {
                    prop_assert_eq!(reader.body(), Some(at));
                    let last = messages.peek().is_none();
                    prop_assert_eq!(
                        event(&mut reader, message),
                        Ok(Event::Body(message.to_vec(), last))
                    );
                    at = at.checked_add(message.len()).expect("the body fits a usize");
                }
                prop_assert_eq!(reader.body(), None);
            }
            if behind {
                prop_assert_eq!(event(&mut reader, &[BEHIND]), Ok(Event::Behind));
            }
        }
    }

    #[test]
    #[should_panic(expected = "an open names at least one channel")]
    fn panics_on_an_open_of_no_channel() {
        drop(Reader::new(&open(0)));
    }
}

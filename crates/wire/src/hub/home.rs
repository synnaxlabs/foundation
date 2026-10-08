use super::{CREDIT, Credit, Error, Mode, Open, keys, rest_of_run};

/// The decoder at the home: it takes each message from the reader's node, in order,
/// and checks the order and the run of the session.
#[derive(Debug, Default)]
pub struct Home {
    next: Next,
}

/// The message the session expects next.
#[derive(Clone, Copy, Debug, Default)]
enum Next {
    #[default]
    Open,
    Keys {
        remain: u32,
        latest: bool,
    },
    Credit,
    /// After the keys run of a latest session, which takes no credit.
    Latest,
}

/// A message from the reader's node, decoded.
#[derive(Clone, Debug)]
pub enum FromReader<'m> {
    /// The open of the session. The run of its keys follows.
    Open(Open),
    /// One message of the keys run.
    Keys {
        /// The keys in the message.
        keys: keys::Iter<'m>,
        /// The run ends with this message.
        last: bool,
    },
    /// A grant of credit.
    Credit(Credit),
}

/// A message from the reader's node that is not part of a run.
enum Message {
    Open(Open),
    Credit(Credit),
}

impl Home {
    /// Decodes the next message from the reader's node.
    ///
    /// # Errors
    ///
    /// The [`Error`] of a message that does not decode, or that breaks the order or
    /// the run of the session: [`Error::Unopened`] for a credit before the open,
    /// [`Error::Reopen`] for a second open, [`Error::Latest`] for a credit in a latest
    /// session, and [`Error::Run`] for a message with more keys than remain. A message
    /// of a run has no kind, so a message where the run continues is read as one. The
    /// session is then not valid ([`MALFORMED`](crate::header::MALFORMED)), and the
    /// caller stops it.
    pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<FromReader<'m>, Error> {
        let (event, next) = match self.next {
            Next::Keys { remain, latest } => {
                let keys = keys::decode(message)?;
                let remain = rest_of_run(remain, keys.len())?;
                let last = remain == 0;
                let next = match (last, latest) {
                    (false, _) => Next::Keys { remain, latest },
                    (true, false) => Next::Credit,
                    (true, true) => Next::Latest,
                };
                (FromReader::Keys { keys, last }, next)
            }
            Next::Open | Next::Credit | Next::Latest => {
                match (Message::decode(message)?, self.next) {
                    (Message::Open(open), Next::Open) => (
                        FromReader::Open(open),
                        Next::Keys {
                            remain: open.channels,
                            latest: open.mode == Mode::Latest,
                        },
                    ),
                    (Message::Credit(_), Next::Open) => {
                        return Err(Error::Unopened { kind: CREDIT });
                    }
                    (Message::Credit(_), Next::Latest) => {
                        return Err(Error::Latest { kind: CREDIT });
                    }
                    (Message::Open(open), _) => {
                        return Err(Error::Reopen { kind: open.kind() });
                    }
                    (Message::Credit(credit), _) => {
                        (FromReader::Credit(credit), Next::Credit)
                    }
                }
            }
        };
        self.next = next;
        Ok(event)
    }
}

impl Message {
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.first() == Some(&CREDIT) {
            Credit::decode(bytes).map(Self::Credit)
        } else {
            Open::decode(bytes).map(Self::Open)
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::channel;

    use super::*;
    use crate::hub::tests::{cut, encode_credit, encode_keys, encode_open, key};

    fn open(channels: u32) -> Vec<u8> {
        encode_open(Open {
            mode: Mode::Latest,
            channels,
        })
    }

    fn credit(limit_bytes: u64) -> Vec<u8> {
        encode_credit(Credit { limit_bytes })
    }

    /// A home whose complete session opened with `keys`, after the keys run.
    fn opened(keys: &[channel::Key]) -> Home {
        let mut home = Home::default();
        let channels = u32::try_from(keys.len()).expect("the keys fit a u32");
        let open = encode_open(Open {
            mode: Mode::Complete { limit_bytes: 0 },
            channels,
        });
        home.decode(&open).expect("the open decodes");
        home.decode(&encode_keys(keys)).expect("the keys decode");
        home
    }

    #[derive(Debug, PartialEq)]
    enum Event {
        Open(Open),
        Keys(Vec<channel::Key>, bool),
        Credit(Credit),
    }

    fn event(home: &mut Home, message: &[u8]) -> Result<Event, Error> {
        home.decode(message).map(|event| match event {
            FromReader::Open(open) => Event::Open(open),
            FromReader::Keys { keys, last } => Event::Keys(keys.collect(), last),
            FromReader::Credit(credit) => Event::Credit(credit),
        })
    }

    #[test]
    fn decodes_each_message_of_a_session() {
        let mut home = Home::default();
        let complete = Open {
            mode: Mode::Complete { limit_bytes: 64 },
            channels: 3,
        };
        assert_eq!(
            event(&mut home, &encode_open(complete)),
            Ok(Event::Open(complete))
        );
        let keys = encode_keys(&[key(5), key(6), key(7)]);
        let (first, second) = keys.split_at(keys::LEN);
        assert_eq!(
            event(&mut home, first),
            Ok(Event::Keys(vec![key(5)], false))
        );
        assert_eq!(
            event(&mut home, second),
            Ok(Event::Keys(vec![key(6), key(7)], true))
        );
        for limit_bytes in [128, 256] {
            assert_eq!(
                event(&mut home, &credit(limit_bytes)),
                Ok(Event::Credit(Credit { limit_bytes }))
            );
        }
    }

    #[test]
    fn refuses_a_second_open() {
        let mut home = opened(&[key(1)]);
        assert_eq!(home.decode(&open(1)).err(), Some(Error::Reopen { kind: 1 }));
        let complete = encode_open(Open {
            mode: Mode::Complete { limit_bytes: 1 },
            channels: 1,
        });
        assert_eq!(
            home.decode(&complete).err(),
            Some(Error::Reopen { kind: 2 })
        );
        assert_eq!(
            event(&mut home, &credit(1)),
            Ok(Event::Credit(Credit { limit_bytes: 1 }))
        );
    }

    #[test]
    fn refuses_a_credit_in_a_latest_session() {
        let mut home = Home::default();
        home.decode(&open(1)).expect("the open decodes");
        home.decode(&encode_keys(&[key(1)]))
            .expect("the key decodes");
        for _ in 0..2 {
            assert_eq!(
                home.decode(&credit(1)).err(),
                Some(Error::Latest { kind: 3 })
            );
        }
        assert_eq!(home.decode(&open(1)).err(), Some(Error::Reopen { kind: 1 }));
    }

    #[test]
    fn refuses_a_credit_before_the_open() {
        let mut home = Home::default();
        assert_eq!(
            home.decode(&credit(1)).err(),
            Some(Error::Unopened { kind: 3 })
        );
        assert_eq!(
            event(&mut home, &open(1)),
            Ok(Event::Open(Open {
                mode: Mode::Latest,
                channels: 1
            }))
        );
    }

    #[test]
    fn decodes_the_message_before_the_order() {
        assert_eq!(
            Home::default().decode(&[CREDIT, 0]).err(),
            Some(Error::Length { len: 2 })
        );
        assert_eq!(
            opened(&[key(1)]).decode(&[1, 0]).err(),
            Some(Error::Length { len: 2 })
        );
        assert_eq!(
            opened(&[key(1)]).decode(&[9]).err(),
            Some(Error::Kind { kind: 9 })
        );
    }

    #[test]
    fn refuses_an_open_of_no_channel() {
        assert_eq!(
            Home::default().decode(&[1, 0, 0, 0, 0]).err(),
            Some(Error::Channels)
        );
    }

    #[test]
    fn refuses_a_run_message_with_more_keys_than_remain() {
        let mut home = Home::default();
        home.decode(&open(2)).expect("the open decodes");
        let three = encode_keys(&[key(1), key(2), key(3)]);
        assert_eq!(
            home.decode(&three).err(),
            Some(Error::Run {
                items: 3,
                remain: 2
            })
        );
        assert_eq!(
            event(&mut home, &three[..32]),
            Ok(Event::Keys(vec![key(1), key(2)], true))
        );
    }

    #[test]
    fn reads_a_credit_before_the_keys_run_ends_as_keys() {
        let mut home = Home::default();
        home.decode(&open(2)).expect("the open decodes");
        home.decode(&encode_keys(&[key(1)]))
            .expect("the first key decodes");
        assert_eq!(
            home.decode(&credit(1)).err(),
            Some(Error::Length { len: 9 })
        );
    }

    fn mode() -> impl Strategy<Value = Mode> {
        prop_oneof![
            Just(Mode::Latest),
            any::<u64>().prop_map(|limit_bytes| Mode::Complete { limit_bytes }),
        ]
    }

    proptest! {
        #[test]
        fn decodes_a_session_cut_into_messages_of_any_size(
            mode in mode(),
            bits in proptest::collection::vec(any::<u128>(), 1..40),
            credits in proptest::collection::vec(any::<u64>(), 0..4),
            sizes in proptest::collection::vec(1..=8_usize, 1..8),
        ) {
            let keys: Vec<_> = bits.into_iter().map(key).collect();
            let open = Open {
                mode,
                channels: u32::try_from(keys.len()).expect("the keys fit a u32"),
            };
            let mut home = Home::default();
            prop_assert_eq!(event(&mut home, &encode_open(open)), Ok(Event::Open(open)));
            let mut messages = cut(&keys, &mut sizes.into_iter().cycle())
                .into_iter()
                .peekable();
            while let Some(message) = messages.next() {
                let last = messages.peek().is_none();
                prop_assert_eq!(
                    event(&mut home, &encode_keys(message)),
                    Ok(Event::Keys(message.to_vec(), last))
                );
            }
            for limit_bytes in credits {
                let expected = if mode == Mode::Latest {
                    Err(Error::Latest { kind: CREDIT })
                } else {
                    Ok(Event::Credit(Credit { limit_bytes }))
                };
                prop_assert_eq!(event(&mut home, &credit(limit_bytes)), expected);
            }
        }
    }

    #[test]
    fn refuses_an_empty_message_at_each_step() {
        let mut home = Home::default();
        assert_eq!(home.decode(&[]).err(), Some(Error::Empty));
        home.decode(&open(1)).expect("the open decodes");
        assert_eq!(home.decode(&[]).err(), Some(Error::Empty));
        home.decode(&encode_keys(&[key(1)]))
            .expect("the key decodes");
        assert_eq!(home.decode(&[]).err(), Some(Error::Empty));
    }
}

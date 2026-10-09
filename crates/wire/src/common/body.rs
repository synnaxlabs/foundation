//! The count of a body in transit: messages back to back, none empty, that hold
//! exactly the bytes of the body.

/// The rest of one body.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Count {
    len: usize,
    remain: usize,
}

/// Why a [`Count`] refused a message or the end of its stream. Each codec maps it to
/// its own error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The message is empty.
    Empty,
    /// The message has `len` bytes, more than the `remain` of the body.
    Over { len: usize, remain: usize },
    /// The stream ended with `remain` bytes of the body to come.
    Unfinished { remain: usize },
}

impl Count {
    /// The count of a body of `len` bytes.
    pub(crate) fn new(len: usize) -> Self {
        Self { len, remain: len }
    }

    /// Takes `message` as the next part of the body. Gives whether the body ends
    /// with it.
    pub(crate) fn take(&mut self, message: &[u8]) -> Result<bool, Error> {
        let (len, remain) = (message.len(), self.remain);
        if len == 0 {
            return Err(Error::Empty);
        }
        self.remain = remain.checked_sub(len).ok_or(Error::Over { len, remain })?;
        Ok(self.remain == 0)
    }

    /// The bytes that remain.
    pub(crate) fn remain(&self) -> usize {
        self.remain
    }

    /// Where in the body the next message starts.
    pub(crate) fn at(&self) -> usize {
        self.len
            .checked_sub(self.remain)
            .expect("invariant: the rest of the body is no longer than the body")
    }

    /// Checks that the body ended, when its stream ends.
    pub(crate) fn end(&self) -> Result<(), Error> {
        match self.remain {
            0 => Ok(()),
            remain => Err(Error::Unfinished { remain }),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn refuses_an_empty_message_and_keeps_the_rest() {
        let mut count = Count::new(4);
        assert_eq!(count.take(&[]), Err(Error::Empty));
        assert_eq!((count.remain(), count.at()), (4, 0));
    }

    #[test]
    fn refuses_a_message_past_the_rest_and_keeps_the_rest() {
        let mut count = Count::new(4);
        assert_eq!(count.take(&[0; 3]), Ok(false));
        assert_eq!(count.take(&[0; 2]), Err(Error::Over { len: 2, remain: 1 }));
        assert_eq!((count.remain(), count.at()), (1, 3));
        assert_eq!(count.take(&[0]), Ok(true));
        assert_eq!(count.take(&[0]), Err(Error::Over { len: 1, remain: 0 }));
    }

    #[test]
    fn ends_unfinished_while_bytes_remain() {
        let mut count = Count::new(4);
        assert_eq!(count.end(), Err(Error::Unfinished { remain: 4 }));
        assert_eq!(count.take(&[0; 3]), Ok(false));
        assert_eq!(count.end(), Err(Error::Unfinished { remain: 1 }));
        assert_eq!(count.take(&[0]), Ok(true));
        assert_eq!(count.end(), Ok(()));
        assert_eq!(Count::new(0).end(), Ok(()));
    }

    proptest! {
        #[test]
        fn ends_at_the_last_part_of_any_cut(
            parts in proptest::collection::vec(1..40_usize, 1..8),
        ) {
            let len = parts.iter().sum();
            let mut count = Count::new(len);
            let mut rest = len;
            for &part in &parts {
                prop_assert_eq!(count.at(), len.checked_sub(rest).expect("rest fits"));
                rest = rest.checked_sub(part).expect("each part is in the body");
                prop_assert_eq!(count.take(&vec![0; part]), Ok(rest == 0));
                prop_assert_eq!(count.remain(), rest);
            }
        }
    }
}

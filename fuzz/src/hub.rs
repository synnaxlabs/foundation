//! What the targets of a hub session have in common.

use wire::hub::Error;

/// The stream messages in a hub input: each is a length byte and then that many bytes.
/// The last message ends with the input.
pub fn messages(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (&len, rest) = bytes.split_first()?;
        let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
        bytes = rest;
        Some(message)
    })
}

/// The rest of a run of items in a hub session, kept apart from the decoder.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    width: usize,
    remain: u32,
}

impl Run {
    /// A run of `items` items of `width` bytes each.
    #[must_use]
    pub fn new(width: usize, items: u32) -> Self {
        Self {
            width,
            remain: items,
        }
    }

    /// The rest of the run after a message of `items` items that the decoder took and
    /// called `last`. `None` when the run ended.
    ///
    /// # Panics
    ///
    /// When the message has no item or more items than remain, or when `last` is not
    /// where the run ends.
    #[must_use]
    pub fn take(self, items: usize, last: bool) -> Option<Self> {
        assert!(items > 0, "a run message has no item");
        let remain = u32::try_from(items)
            .ok()
            .and_then(|items| self.remain.checked_sub(items))
            .expect("a run message has more items than remain");
        assert_eq!(last, remain == 0, "the run ends at another message");
        (!last).then_some(Self { remain, ..self })
    }

    /// Whether `error` is the refusal of `message` where the run continues. Only one
    /// error is correct, and a message that the run takes has none.
    #[must_use]
    pub fn refused(self, message: &[u8], error: Error) -> bool {
        let len = message.len();
        if len == 0 {
            error == Error::Empty
        } else if !len.is_multiple_of(self.width) {
            error == Error::Length { len }
        } else {
            let (items, remain) = (len / self.width, self.remain);
            u32::try_from(items).is_ok_and(|items| items > remain)
                && error == Error::Run { items, remain }
        }
    }
}

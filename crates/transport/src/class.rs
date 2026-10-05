/// The traffic class of a stream, from the highest priority to the lowest. A class
/// sets the stream's priority and the carrier it prefers. Streams of one class share
/// the link fairly.
///
/// ```
/// use transport::{Class, Error, Session};
///
/// async fn push(session: &Session, frame: block::Block) -> Result<(), Error> {
///     let mut sender = session.open_sender(Class::Latest).await?;
///     sender.send(frame).await?;
///     sender.finish()
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// The highest priority, for small messages that must arrive soon.
    Command,
    /// Second, with a small send buffer, so a message that a newer one replaces does
    /// not wait behind much.
    Latest,
    /// Third, for ordered bulk that must arrive whole.
    Complete,
    /// The lowest, for bulk that waits for spare capacity.
    CatchUp,
}

impl Class {
    /// Every class, by [`Class::byte`].
    pub(crate) const ALL: [Self; 4] =
        [Self::Command, Self::Latest, Self::Complete, Self::CatchUp];

    /// The byte that starts a stream of this class on the wire.
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Command => 0,
            Self::Latest => 1,
            Self::Complete => 2,
            Self::CatchUp => 3,
        }
    }

    /// The class whose streams start with `byte`, if any.
    pub(crate) fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.get(usize::from(byte)).copied()
    }
}

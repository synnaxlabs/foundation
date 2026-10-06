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
    /// Third, for ordered bulk that must arrive whole. While streams of both `Latest`
    /// and `Complete` hold messages to send, `Complete` gets 3 bytes of each 4 that
    /// the carrier takes. A message that `try_send` gave back is not held.
    Complete,
    /// The lowest, for bulk that waits for spare capacity.
    CatchUp,
}

impl Class {
    /// The class's place in priority order: 0 for the highest.
    pub(crate) fn rank(self) -> usize {
        match self {
            Self::Command => 0,
            Self::Latest => 1,
            Self::Complete => 2,
            Self::CatchUp => 3,
        }
    }
}

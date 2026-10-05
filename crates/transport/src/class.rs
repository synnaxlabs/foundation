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

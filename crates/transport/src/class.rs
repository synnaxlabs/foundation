/// The traffic class of a stream, from the highest priority to the lowest. A class
/// sets the stream's priority and the carrier it prefers. Streams of one class share
/// the link fairly.
///
/// ```
/// use transport::{Class, Error, Session};
///
/// async fn push(session: &Session, frame: block::Block) -> Result<(), Error> {
///     let mut sender = session.open_sender(Class::Latest).await?;
///     sender.send(frame).await
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Commands, their acknowledgments, and consensus messages.
    Command,
    /// Latest-mode frames. A newer frame usually cancels an older stream.
    Latest,
    /// Complete-mode frames and replication.
    Complete,
    /// Catch-up reads from disk and blob fetches.
    CatchUp,
}

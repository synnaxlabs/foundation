/// Why a session closed or a stream was cancelled. The protocol above gives each value
/// its meaning; the transport only carries it. Code 0 means no reason was given. A peer
/// that sends a code over 32 bits breaks the protocol, and the session ends with
/// [`Error::Broken`](crate::Error::Broken).
///
/// ```
/// const SUPERSEDED: transport::Code = transport::Code(1);
///
/// fn cancel(sender: transport::stream::Sender) {
///     sender.reset(SUPERSEDED);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Code(pub u32);

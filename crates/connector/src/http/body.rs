//! A request body that is one buffer.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

/// The whole body, until `hyper` takes it.
pub(super) struct Whole(pub(super) Option<Bytes>);

impl Body for Whole {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let bytes = self.get_mut().0.take().filter(|bytes| !bytes.is_empty());
        Poll::Ready(bytes.map(|bytes| Ok(Frame::data(bytes))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.as_ref().is_none_or(Bytes::is_empty)
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.0.as_ref().map_or(0, Bytes::len);
        SizeHint::with_exact(u64::try_from(len).expect("a length fits in 64 bits"))
    }
}

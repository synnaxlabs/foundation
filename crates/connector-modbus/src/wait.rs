//! Waits that end at a deadline.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use env::clock::Clock;
use types::time::Monotonic;

/// Polls `poll` until it is ready and gives its value, or gives `None` at
/// `deadline`. With no deadline, it waits without end.
pub(crate) async fn within<T>(
    clock: &Clock,
    deadline: Option<Monotonic>,
    mut poll: impl FnMut(&mut Context<'_>) -> Poll<T>,
) -> Option<T> {
    let mut sleep = deadline.map(|deadline| clock.sleep_until(deadline));
    poll_fn(|cx| {
        if let Poll::Ready(value) = poll(cx) {
            return Poll::Ready(Some(value));
        }
        match &mut sleep {
            Some(sleep) => Pin::new(sleep).poll(cx).map(|()| None),
            None => Poll::Pending,
        }
    })
    .await
}

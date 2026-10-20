//! The home end of a session whose serve future a test or bench polls by hand.

use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use env::tasks::Tasks;
use hub::{Hub, Link, Served, serve};
use transport::{Session, Transport};

/// Accepts the session of the next program on `transport`, and gives it and its link
/// on `hub`. A task in `tasks` serves its first stream, its hello.
pub(crate) async fn program(
    transport: &Transport,
    hub: &Hub,
    tasks: &Tasks,
) -> (Session, Link) {
    let (session, hello) = crate::net::accept(transport).await;
    let link = hub.link(session.clone());
    let served = link.clone();
    tasks.spawn(async move { drop(served.serve(hello).await) });
    (session, link)
}

/// One poll of `serving` with a waker that does nothing.
///
/// # Panics
///
/// When `serving` ends.
pub(crate) fn poll(
    serving: Pin<&mut impl Future<Output = Result<Served, serve::Error>>>,
) {
    let mut cx = Context::from_waker(Waker::noop());
    if let Poll::Ready(served) = serving.poll(&mut cx) {
        panic!("the serve ended: {served:?}");
    }
}

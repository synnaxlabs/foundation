//! The tests that bound the heap of transport with one count of the bytes it holds.
//! The count covers each thread, so this binary has no test harness. The sim runs on
//! one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::OnceCell;
use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;

use block::Block;
use env::clock::Clock;
use transport::Error;
use transport::stream::Receiver;
use types::time::Span;

#[path = "../common/mod.rs"]
mod common;
mod dials;
mod failed;
mod held;
mod kept;
mod opened;
mod peers;
mod stretch;

/// The heap of the cell that holds a closed session's error, which the drop of its
/// receiver frees: an `Rc` box, with its two counts.
pub(crate) const CLOSED: usize = 2 * size_of::<usize>() + size_of::<OnceCell<Error>>();

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

fn main() {
    held::main();
    kept::main();
    opened::main();
    failed::main();
    stretch::main();
    peers::main();
    dials::main();
}

/// Polls one `receiver.recv()` each millisecond until it is ready, and calls
/// `on_pending` with the count of polls that gave `Pending` after each one. Gives the
/// read and that count.
pub(crate) async fn next(
    receiver: &mut Receiver,
    clock: &Clock,
    mut on_pending: impl FnMut(usize),
) -> (Result<Option<Block>, Error>, usize) {
    let mut recv = pin!(receiver.recv());
    let mut pending = 0;
    loop {
        if let Poll::Ready(read) =
            poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx))).await
        {
            return (read, pending);
        }
        pending += 1;
        on_pending(pending);
        clock.sleep(Span::MILLISECOND).await;
    }
}

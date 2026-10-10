//! A request that the hub refuses for its length allocates no body. This binary has no
//! test harness: the peak covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/agent.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod agent;
#[path = "../common/mod.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod common;
#[path = "../common/net.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod net;
#[path = "../common/node.rs"]
mod node;
#[path = "../common/sessions.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod sessions;

use std::sync::{Arc, Mutex};

use agent::{SUBJECT, name, reset_with};
use hub::serve;
use sessions::{closed_after, sessions};
use types::time::Span;
use wire::hub::BUSY;
use wire::hub::client::BODY_BYTES_MAX;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

fn main() {
    refuses_a_request_over_the_share_before_it_allocates_the_body();
}

/// While a request of 1 byte is open, a request of `BODY_BYTES_MAX` of the same
/// subject is over the share, and stops with `BUSY` before the hub allocates its body.
fn refuses_a_request_over_the_share_before_it_allocates_the_body() {
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let open = async |node: &sim::node::Node, tasks: &env::tasks::Tasks| {
        (common::hub(node, tasks.clone()).await.0, ())
    };
    let served = sessions(
        1,
        [SUBJECT, SUBJECT],
        open,
        |[mut first, mut second]| async move {
            first.admit().await;
            second.admit().await;
            let _held = first.unfinished(1, &[]).await;
            first.sleep(Span::MILLISECOND).await;
            ALLOCATOR.reset_peak();
            let held = ALLOCATOR.held();
            let mut busy = second.unfinished(BODY_BYTES_MAX, &[]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY), "the hub refuses it");
            *kept.lock().expect("not poisoned") = Some((held, ALLOCATOR.peak()));
        },
    );
    let refused = serve::Error::Share {
        subject: name(SUBJECT),
        length: BODY_BYTES_MAX,
        held: 1,
    };
    assert_eq!(
        served,
        closed_after(vec![Err(refused)], 2, 1),
        "refused by the share"
    );
    let (held, peak) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    let body = usize::try_from(BODY_BYTES_MAX).expect("16 MiB is a usize");
    assert!(
        peak - held < body,
        "the refusal raised the bytes held by {} from {held}, a body of {body}",
        peak - held,
    );
}

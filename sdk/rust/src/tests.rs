use std::net::Ipv6Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::runtime::Builder;
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Metadata, Subscriber};

use super::*;

#[test]
fn a_client_moves_to_and_is_shared_between_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Client>();
}

#[test]
fn the_pool_holds_a_datagram() {
    let pool = block::Pool::heap(POOL);
    assert!(pool.largest() >= 1472, "{}", pool.largest());
}

#[test]
fn the_frames_pool_holds_a_frame_of_32_kib() {
    let pool = block::Pool::heap(FRAMES);
    assert!(pool.largest() >= 32 << 10, "{}", pool.largest());
}

#[test]
fn each_error_names_its_cause() {
    let thread = Error::Thread {
        reason: "out of memory".into(),
    };
    let bind = Error::Bind(transport::Error::TimedOut);
    let session = Error::Session(program::Error::Unanswered);
    assert_eq!(
        [thread, bind, session, Error::Panicked].map(|e| e.to_string()),
        [
            "cannot start the client thread: out of memory",
            "cannot bind the local UDP socket: the peer stopped answering",
            "the node finished a stream with no answer",
            "the client thread panicked",
        ]
    );
}

// These tests build the client through the private `Client::start`, with a `main`
// that panics or fails. `connect` is `start` with `run` as its `main`, and no public
// input makes a correct client's thread panic.

/// A client whose thread sends `Ok`, waits for its end, and then runs `after`.
fn client(after: impl FnOnce() -> Result<(), Error> + Send + 'static) -> Client {
    let started = Client::start(|_, outcome, ended| async move {
        outcome.send(Ok(())).unwrap();
        drop(ended.await);
        if let Err(error) = after() {
            outcome.send(Err(error)).unwrap();
        }
    });
    started.expect("a client")
}

#[test]
fn a_close_ends_the_thread_and_waits_for_it() {
    let (ended, end) = mpsc::channel();
    let client = client(move || {
        ended.send(()).unwrap();
        Ok(())
    });
    assert_eq!(client.close(), Ok(()));
    assert_eq!(end.try_recv(), Ok(()));
}

#[test]
fn a_close_gives_the_error_that_ended_the_session() {
    let failed = Error::Session(program::Error::Unanswered);
    let error = failed.clone();
    let client = client(move || Err(error));
    assert_eq!(client.close(), Err(failed));
}

#[test]
fn a_close_gives_panicked_when_the_thread_panicked() {
    let client = client(|| panic!("the session failed"));
    assert_eq!(client.close(), Err(Error::Panicked));
}

#[test]
fn a_close_gives_panicked_when_a_task_panicked() {
    let started = Client::start(|tasks, outcome, _| async move {
        outcome.send(Ok(())).unwrap();
        tasks.spawn(async { panic!("a task failed") });
        std::future::pending::<()>().await;
    });
    let client = started.expect("a client");
    assert_eq!(client.close(), Err(Error::Panicked));
}

#[test]
fn a_drop_ends_the_thread_and_does_not_wait() {
    let (go, wait) = mpsc::channel::<()>();
    let (ended, end) = mpsc::channel();
    let client = client(move || {
        #[expect(clippy::disallowed_methods, reason = "the thread waits for the test")]
        wait.recv().unwrap();
        ended.send(()).unwrap();
        Ok(())
    });
    drop(client);
    go.send(()).unwrap();
    #[expect(clippy::disallowed_methods, reason = "the test waits for the thread")]
    let outcome = end.recv_timeout(Duration::from_secs(10));
    assert_eq!(outcome, Ok(()));
}

#[test]
fn a_connect_gives_the_error_that_the_thread_sends() {
    let failed = Error::Session(program::Error::Unanswered);
    let error = failed.clone();
    let started = Client::start(move |_, outcome, _| async move {
        outcome.send(Err(error)).unwrap();
    });
    assert_eq!(started.err(), Some(failed));
}

#[test]
fn a_connect_gives_panicked_when_the_thread_panics_after_its_error() {
    let started = Client::start(|_, outcome, _| async move {
        let error = Error::Session(program::Error::Unanswered);
        outcome.send(Err(error)).unwrap();
        panic!("the session failed");
    });
    assert_eq!(started.err(), Some(Error::Panicked));
}

#[test]
fn a_connect_gives_panicked_when_the_thread_panics_before_its_outcome() {
    let started = Client::start(|_, _, _| async { panic!("the session failed") });
    assert_eq!(started.err(), Some(Error::Panicked));
}

/// Counts the events that reach it. Clones share the count.
#[derive(Clone, Default)]
struct Count(Arc<AtomicUsize>);

impl Subscriber for Count {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, _: &Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[test]
fn the_thread_logs_to_the_dispatch_of_the_caller_of_connect() {
    let count = Count::default();
    let client =
        tracing::dispatcher::with_default(&Dispatch::new(count.clone()), || {
            Client::start(|_, outcome, _| async move {
                tracing::info!("the client started");
                outcome.send(Ok(())).unwrap();
            })
        });
    assert_eq!(client.expect("a client").close(), Ok(()));
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
}

/// Connects to a node at `addresses`, and checks that the error names each one with
/// `cause`.
fn unreachable(addresses: Vec<SocketAddr>, cause: &transport::Error) {
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        addresses: addresses.clone(),
        key: types::node::Key::from_u128(1),
        public_key: peer,
    };
    let subject = Subject {
        name: "plant.operator".parse().unwrap(),
        private_key: PrivateKey([8; 32]),
    };
    let attempts = addresses
        .into_iter()
        .map(|address| (transport::Address::Udp(address), cause.clone()))
        .collect();
    let Err(error) = Client::connect(&node, subject) else {
        panic!("a client of an unreachable node connected");
    };
    let unreachable = transport::Error::Unreachable { peer, attempts };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
}

/// Port 0 of IPv6 loopback.
fn port_0() -> SocketAddr {
    SocketAddr::from((Ipv6Addr::LOCALHOST, 0))
}

#[test]
fn a_client_of_port_0_is_unroutable() {
    unreachable(vec![port_0()], &transport::Error::Unroutable);
}

#[test]
fn a_client_of_a_node_with_no_address_is_unreachable_with_no_attempt() {
    unreachable(vec![], &transport::Error::Unroutable);
}

#[test]
fn a_connect_inside_a_tokio_runtime_blocks_and_gives_its_error() {
    let runtime = Builder::new_current_thread().build().unwrap();
    runtime.block_on(async {
        unreachable(vec![port_0()], &transport::Error::Unroutable);
    });
}

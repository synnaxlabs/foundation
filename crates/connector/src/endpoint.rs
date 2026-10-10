//! Endpoints that the connectors on one node share.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll, Waker};

use document::diagnostic::{Code, Diagnostic};
use types::wait;

use crate::kind::Error;

const SETTINGS: Code = Code::new("connector.endpoint-settings");

/// Keeps at most one open endpoint per key on a node, shared by every connector that
/// names that key. `node` makes one for each kind that needs it and gives it to the
/// kind. Any thread may use it.
pub struct Registry<K, S, T>(Arc<Locked<K, S, T>>);

type Locked<K, S, T> = Mutex<Table<K, S, T>>;

struct Table<K, S, T> {
    slots: BTreeMap<K, Slot<S, T>>,
    /// The place of the next [`Waiter`]. It is unique in the registry, so a slot that
    /// frees and goes busy again never holds two waiters with one place.
    next: u64,
}

enum Slot<S, T> {
    /// An open or a close runs, while the [`Waiter`]s in the set wait.
    Busy(wait::Set),
    Open {
        settings: S,
        endpoint: Weak<T>,
    },
}

/// What an acquire found in the slot of its key.
enum Claim<T> {
    Shared(Arc<T>),
    /// The slot was empty and is now busy with this acquire's open.
    Mine,
}

impl<K: Ord + Clone + fmt::Debug, S: PartialEq, T> Registry<K, S, T> {
    /// Makes an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Table {
            slots: BTreeMap::new(),
            next: 0,
        })))
    }

    /// Returns a lease on the endpoint at `key`. When none is open, calls `open` with
    /// `settings` and keeps the endpoint while a lease on it lives. Opens of one key
    /// run one at a time: a second call waits for the first open, then shares its
    /// endpoint, or opens again itself when that open failed or was dropped. Opens of
    /// different keys run at the same time. Safe to drop at any time.
    ///
    /// # Errors
    ///
    /// - [`Error::Config`] with code `connector.endpoint-settings` when the endpoint
    ///   at `key` is open with settings that are not equal to `settings`.
    /// - The error of `open`.
    ///
    /// # Panics
    ///
    /// Only on a bug in this module: an open slot whose endpoint is gone.
    pub async fn acquire<F>(
        &self,
        key: K,
        settings: S,
        open: impl FnOnce(&S) -> F,
    ) -> Result<Lease<K, S, T>, Error>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let claim = Waiter {
            table: &self.0,
            key: &key,
            settings: &settings,
            place: None,
        }
        .await?;
        let endpoint = match claim {
            Claim::Shared(endpoint) => endpoint,
            Claim::Mine => {
                let guard = Free {
                    table: &self.0,
                    key: Some(&key),
                };
                let endpoint = Arc::new(open(&settings).await?);
                let mut table = lock(&self.0);
                let open = Slot::Open {
                    settings,
                    endpoint: Arc::downgrade(&endpoint),
                };
                let busy = table.slots.insert(key.clone(), open);
                drop(table);
                wake(busy);
                guard.disarm();
                endpoint
            }
        };
        Ok(Lease {
            endpoint: Some(endpoint),
            key,
            table: Arc::clone(&self.0),
        })
    }
}

impl<K: Ord + Clone + fmt::Debug, S: PartialEq, T> Default for Registry<K, S, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug, S, T> fmt::Debug for Registry<K, S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(lock(&self.0).slots.keys()).finish()
    }
}

/// A share of one open endpoint. Derefs to the endpoint. When the last lease on it
/// drops, the endpoint drops (closes) on that thread, after the registry's lock is
/// released, and the next [`Registry::acquire`] of its key waits for the close. The
/// last lease often drops on a shard, so the endpoint's drop must not block: an
/// endpoint with a slow close hands the close to its own thread.
///
/// A lease is not `Clone`: acquire again to share the endpoint.
///
/// ```compile_fail
/// fn cloneable<T: Clone>() {}
/// cloneable::<connector::endpoint::Lease<u8, (), ()>>();
/// ```
pub struct Lease<K: Ord, S, T> {
    /// `None` only inside `drop`.
    endpoint: Option<Arc<T>>,
    key: K,
    table: Arc<Locked<K, S, T>>,
}

impl<K: Ord, S, T> Deref for Lease<K, S, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.endpoint
            .as_deref()
            .expect("a lease holds its endpoint until it drops")
    }
}

impl<K: Ord, S, T> Drop for Lease<K, S, T> {
    fn drop(&mut self) {
        let mut table = lock(&self.table);
        let Some(endpoint) = self.endpoint.take() else {
            return;
        };
        if Arc::strong_count(&endpoint) > 1 {
            // Under the lock, so that two last drops never both see a count above 1.
            // Only `acquire` adds a reference, under the lock, so a count of 1
            // means no other lease exists.
            drop(endpoint);
            return;
        }
        if let Some(slot) = table.slots.get_mut(&self.key) {
            *slot = Slot::Busy(wait::Set::new());
        }
        drop(table);
        let _free = Free {
            table: &self.table,
            key: Some(&self.key),
        };
        drop(endpoint);
    }
}

impl<K: Ord + fmt::Debug, S, T> fmt::Debug for Lease<K, S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Frees a busy slot and wakes its waiters: after a close, even one that panicked, or
/// after an open that failed or was dropped.
struct Free<'a, K: Ord, S, T> {
    table: &'a Locked<K, S, T>,
    /// `None` after the open succeeded.
    key: Option<&'a K>,
}

impl<K: Ord, S, T> Free<'_, K, S, T> {
    fn disarm(mut self) {
        self.key = None;
    }
}

impl<K: Ord, S, T> Drop for Free<'_, K, S, T> {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            let busy = lock(self.table).slots.remove(key);
            wake(busy);
        }
    }
}

/// Waits while the slot of `key` is busy, then claims it. It keeps the waker of its
/// last poll in the busy slot, and its drop takes that waker out.
struct Waiter<'a, K: Ord, S, T> {
    table: &'a Locked<K, S, T>,
    key: &'a K,
    settings: &'a S,
    /// The place of its waker in a busy slot, from its first wait until it drops.
    place: Option<u64>,
}

impl<K: Ord + Clone + fmt::Debug, S: PartialEq, T> Future for Waiter<'_, K, S, T> {
    type Output = Result<Claim<T>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut guard = lock(this.table);
        let table = &mut *guard;
        let slot = match table.slots.entry(this.key.clone()) {
            Entry::Vacant(vacant) => {
                vacant.insert(Slot::Busy(wait::Set::new()));
                return Poll::Ready(Ok(Claim::Mine));
            }
            Entry::Occupied(occupied) => occupied.into_mut(),
        };
        match slot {
            Slot::Busy(wakers) => {
                let place = *this.place.get_or_insert_with(|| {
                    let place = table.next;
                    table.next += 1;
                    place
                });
                let replaced = wakers.insert(place, cx.waker());
                drop(guard);
                drop(replaced);
                Poll::Pending
            }
            Slot::Open { settings, endpoint } => {
                let endpoint = endpoint
                    .upgrade()
                    .expect("a lease removes its slot before its endpoint drops");
                if settings == this.settings {
                    Poll::Ready(Ok(Claim::Shared(endpoint)))
                } else {
                    Poll::Ready(Err(unequal(this.key)))
                }
            }
        }
    }
}

impl<K: Ord, S, T> Drop for Waiter<'_, K, S, T> {
    fn drop(&mut self) {
        let Some(place) = self.place else {
            return;
        };
        let mut table = lock(self.table);
        let removed = match table.slots.get_mut(self.key) {
            Some(Slot::Busy(wakers)) => wakers.remove(place),
            _ => None,
        };
        drop(table);
        drop(removed);
    }
}

/// Settings can hold secrets, so the message never shows them.
fn unequal(key: &impl fmt::Debug) -> Error {
    Error::Config(vec![Diagnostic::new(
        SETTINGS,
        None,
        format!("the endpoint {key:?} is open with other settings"),
        "Give every connector on this endpoint the same settings".into(),
    )])
}

fn wake<S, T>(slot: Option<Slot<S, T>>) {
    if let Some(Slot::Busy(wakers)) = slot {
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// Recovers a poisoned lock, so that a drop never panics. No code under the lock
/// leaves the map half changed when it panics.
fn lock<K, S, T>(table: &Locked<K, S, T>) -> MutexGuard<'_, Table<K, S, T>> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::cell::RefCell;
    use std::future;
    use std::pin::pin;
    use std::rc::Rc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;

    use env::clock::Clock;
    use types::time::Span;

    use super::*;
    use crate::common::run;

    /// An endpoint that counts its opens and closes.
    #[derive(Debug)]
    struct Port {
        n: usize,
        closes: Arc<AtomicUsize>,
    }

    impl Drop for Port {
        fn drop(&mut self) {
            self.closes.fetch_add(1, Relaxed);
        }
    }

    /// Opens a [`Port`] after `span`, or fails when `fail` is set.
    #[derive(Clone, Default)]
    struct Opener {
        opens: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    impl Opener {
        async fn open(
            &self,
            clock: &Clock,
            span: Span,
            fail: bool,
        ) -> Result<Port, Error> {
            let n = self.opens.fetch_add(1, Relaxed);
            clock.sleep(span).await;
            if fail {
                return Err(Error::Device("no reply".into()));
            }
            Ok(Port {
                n,
                closes: Arc::clone(&self.closes),
            })
        }

        fn opens(&self) -> usize {
            self.opens.load(Relaxed)
        }

        fn closes(&self) -> usize {
            self.closes.load(Relaxed)
        }
    }

    type Ports = Registry<&'static str, u32, Port>;

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    #[test]
    fn shares_one_open_between_two_acquires() {
        let (a, b, opens) = run(|clock, _, _| async move {
            let ports = Ports::new();
            let opener = Opener::default();
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let a = ports.acquire("tty0", 9600, open).await.expect("opens");
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let b = ports.acquire("tty0", 9600, open).await.expect("shares");
            (a.n, b.n, opener.opens())
        });
        assert_eq!((a, b, opens), (0, 0, 1));
    }

    #[test]
    fn waits_for_an_open_of_the_same_key() {
        let (first, second, opens, elapsed) = run(|clock, tasks, _| async move {
            let ports = Rc::new(Ports::new());
            let opener = Opener::default();
            let first = Rc::new(RefCell::new(None));
            let (p, o, c, slot) = (
                Rc::clone(&ports),
                opener.clone(),
                clock.clone(),
                Rc::clone(&first),
            );
            tasks.spawn(async move {
                let open = |_: &u32| o.open(&c, ms(50), false);
                let lease = p.acquire("tty0", 9600, open).await.expect("opens");
                *slot.borrow_mut() = Some((lease.n, c.now()));
                c.sleep(Span::SECOND).await;
                drop(lease);
            });
            let start = clock.now();
            clock.sleep(ms(10)).await;
            let open = |_: &u32| opener.open(&clock, ms(50), false);
            let lease = ports.acquire("tty0", 9600, open).await.expect("shares");
            let elapsed = clock.now() - start;
            let first = first
                .borrow_mut()
                .take()
                .expect("the first acquire returned");
            (first.0, lease.n, opener.opens(), elapsed)
        });
        assert_eq!((first, second, opens), (0, 0, 1));
        assert_eq!(elapsed, ms(50), "returns when the first open ends");
    }

    #[test]
    fn opens_again_when_the_open_it_waited_for_failed() {
        let (failed, second, opens) = run(|clock, tasks, _| async move {
            let ports = Rc::new(Ports::new());
            let opener = Opener::default();
            let failed = Rc::new(RefCell::new(None));
            let (p, o, c, slot) = (
                Rc::clone(&ports),
                opener.clone(),
                clock.clone(),
                Rc::clone(&failed),
            );
            tasks.spawn(async move {
                let open = |_: &u32| o.open(&c, ms(50), true);
                let error = p.acquire("tty0", 9600, open).await.expect_err("fails");
                *slot.borrow_mut() = Some(error.to_string());
            });
            clock.sleep(ms(10)).await;
            let open = |_: &u32| opener.open(&clock, ms(50), false);
            let lease = ports.acquire("tty0", 9600, open).await.expect("opens");
            let failed = failed.borrow_mut().take().expect("the first returned");
            (failed, lease.n, opener.opens())
        });
        assert_eq!(failed, "the device is in a bad state: no reply");
        assert_eq!((second, opens), (1, 2), "the second acquire opened again");
    }

    #[test]
    fn opens_again_when_the_open_it_waited_for_was_dropped() {
        let (second, opens, elapsed) = run(|clock, tasks, _| async move {
            let ports = Rc::new(Ports::new());
            let opener = Opener::default();
            let token = crate::cancel::Token::new();
            let (p, o, c, t) = (
                Rc::clone(&ports),
                opener.clone(),
                clock.clone(),
                token.clone(),
            );
            tasks.spawn(async move {
                let open = |_: &u32| o.open(&c, Span::SECOND, false);
                let acquired = t.race(p.acquire("tty0", 9600, open)).await;
                assert!(acquired.is_none(), "cancelled during the open");
            });
            let canceller = clock.clone();
            tasks.spawn(async move {
                canceller.sleep(ms(20)).await;
                token.cancel();
            });
            let start = clock.now();
            clock.sleep(ms(10)).await;
            let open = |_: &u32| opener.open(&clock, ms(50), false);
            let lease = ports.acquire("tty0", 9600, open).await.expect("opens");
            (lease.n, opener.opens(), clock.now() - start)
        });
        assert_eq!((second, opens), (1, 2));
        assert_eq!(elapsed, ms(70), "opens at the cancel, then takes 50 ms");
    }

    #[test]
    fn refuses_other_settings_on_an_open_endpoint() {
        let (diagnostics, opens) = run(|clock, _, _| async move {
            let ports = Ports::new();
            let opener = Opener::default();
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let _lease = ports.acquire("tty0", 9600, open).await.expect("opens");
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let error = ports
                .acquire("tty0", 19200, open)
                .await
                .expect_err("refused");
            let Error::Config(diagnostics) = error else {
                panic!("a config error: {error:?}");
            };
            (diagnostics, opener.opens())
        });
        assert_eq!(opens, 1);
        assert_eq!(
            diagnostics,
            [Diagnostic::new(
                SETTINGS,
                None,
                "the endpoint \"tty0\" is open with other settings".into(),
                "Give every connector on this endpoint the same settings".into(),
            )]
        );
    }

    #[test]
    fn closes_on_the_last_release_and_opens_again() {
        let (after_one, after_both, again, opens) = run(|clock, _, _| async move {
            let ports = Ports::new();
            let opener = Opener::default();
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let a = ports.acquire("tty0", 9600, open).await.expect("opens");
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let b = ports.acquire("tty0", 9600, open).await.expect("shares");
            drop(a);
            let after_one = opener.closes();
            drop(b);
            let after_both = opener.closes();
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let c = ports
                .acquire("tty0", 19200, open)
                .await
                .expect("opens again");
            (after_one, after_both, c.n, opener.opens())
        });
        assert_eq!((after_one, after_both), (0, 1));
        assert_eq!((again, opens), (1, 2), "new settings after the close");
    }

    #[test]
    fn opens_other_keys_at_the_same_time() {
        let elapsed = run(|clock, tasks, _| async move {
            let ports = Rc::new(Ports::new());
            let opener = Opener::default();
            let (p, o, c) = (Rc::clone(&ports), opener.clone(), clock.clone());
            tasks.spawn(async move {
                let open = |_: &u32| o.open(&c, ms(50), false);
                let lease = p.acquire("tty0", 9600, open).await.expect("opens");
                c.sleep(Span::SECOND).await;
                drop(lease);
            });
            let start = clock.now();
            clock.sleep(ms(10)).await;
            let open = |_: &u32| opener.open(&clock, ms(50), false);
            let _lease = ports.acquire("tty1", 9600, open).await.expect("opens");
            clock.now() - start
        });
        assert_eq!(elapsed, ms(60), "never waits for tty0");
    }

    /// Counts its wakes. A waker that the registry keeps holds a count of its `Arc`.
    #[derive(Default)]
    struct Count(AtomicUsize);

    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    /// Polls `future` once with a waker of `count`.
    fn poll_with<F: Future + ?Sized>(
        future: Pin<&mut F>,
        count: &Arc<Count>,
    ) -> Poll<F::Output> {
        let waker = Waker::from(Arc::clone(count));
        future.poll(&mut std::task::Context::from_waker(&waker))
    }

    /// An acquire of `tty0` whose open never ends, so the slot stays busy.
    fn busy(
        ports: &Ports,
    ) -> impl Future<Output = Result<Lease<&'static str, u32, Port>, Error>> {
        ports.acquire("tty0", 9600, |_: &u32| future::pending())
    }

    #[test]
    fn acquires_that_wait_and_drop_leave_no_waker() {
        let ports = Ports::new();
        let mut open = pin!(busy(&ports));
        let count = Arc::new(Count::default());
        assert!(poll_with(open.as_mut(), &count).is_pending());
        let count = Arc::new(Count::default());
        for _ in 0..1000 {
            let waiting = pin!(busy(&ports));
            assert!(poll_with(waiting, &count).is_pending());
        }
        assert_eq!(Arc::strong_count(&count), 1);
    }

    #[test]
    fn an_acquire_polled_with_new_wakers_keeps_the_last_one() {
        let ports = Ports::new();
        let mut open = pin!(busy(&ports));
        assert!(poll_with(open.as_mut(), &Arc::new(Count::default())).is_pending());
        let mut waiting = pin!(busy(&ports));
        let counts: Vec<Arc<Count>> = (0..1000).map(|_| Arc::default()).collect();
        for count in &counts {
            assert!(poll_with(waiting.as_mut(), count).is_pending());
        }
        let held: Vec<usize> = counts.iter().map(Arc::strong_count).collect();
        let mut expected = vec![1; 1000];
        expected[999] = 2;
        assert_eq!(held, expected);
    }

    #[test]
    fn a_dropped_acquire_keeps_the_wakers_of_others() {
        let ports = Ports::new();
        let mut open = Box::pin(busy(&ports));
        assert!(poll_with(open.as_mut(), &Arc::new(Count::default())).is_pending());
        let kept = Arc::new(Count::default());
        let mut waiting = pin!(busy(&ports));
        assert!(poll_with(waiting.as_mut(), &kept).is_pending());
        {
            let dropped = pin!(busy(&ports));
            assert!(poll_with(dropped, &Arc::new(Count::default())).is_pending());
        }
        assert_eq!(Arc::strong_count(&kept), 2);
        drop(open);
        assert_eq!(kept.0.load(Relaxed), 1, "the drop of the open wakes it");
    }

    // A waiter keeps its place after its busy span ends. A place used again by a waiter
    // of the next span would let the first waiter's drop take its waker.
    #[test]
    fn a_waiter_of_an_ended_span_keeps_the_wakers_of_the_next_span() {
        let ports = Ports::new();
        let mut first = Box::pin(busy(&ports));
        {
            let mut open = pin!(busy(&ports));
            assert!(poll_with(open.as_mut(), &Arc::new(Count::default())).is_pending());
            assert!(
                poll_with(first.as_mut(), &Arc::new(Count::default())).is_pending()
            );
        }
        let mut open = pin!(busy(&ports));
        let count = Arc::new(Count::default());
        assert!(poll_with(open.as_mut(), &count).is_pending());
        let kept = Arc::new(Count::default());
        let mut waiting = pin!(busy(&ports));
        assert!(poll_with(waiting.as_mut(), &kept).is_pending());
        drop(first);
        assert_eq!(Arc::strong_count(&kept), 2);
    }

    /// Polls `f` to the end on this thread, parked between wakes.
    fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                return out;
            }
            #[expect(clippy::disallowed_methods, reason = "a test owns its threads")]
            std::thread::park();
        }
    }

    /// An endpoint that fails the test when two are open at once.
    struct Exclusive(Arc<AtomicUsize>);

    impl Drop for Exclusive {
        fn drop(&mut self) {
            // A slow close, so that an open during it shows.
            std::thread::yield_now();
            self.0.fetch_sub(1, Relaxed);
        }
    }

    #[test]
    fn never_opens_two_endpoints_of_one_key_across_threads() {
        let ports = Arc::new(Registry::<u8, (), Exclusive>::new());
        let live = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (ports, live) = (Arc::clone(&ports), Arc::clone(&live));
                #[expect(
                    clippy::disallowed_methods,
                    reason = "a test owns its threads"
                )]
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        let open = |(): &()| {
                            let n = live.fetch_add(1, Relaxed);
                            assert_eq!(n, 0, "a second endpoint opened");
                            future::ready(Ok(Exclusive(Arc::clone(&live))))
                        };
                        drop(block_on(ports.acquire(0, (), open)).expect("opens"));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no thread panicked");
        }
        assert_eq!(live.load(Relaxed), 0, "every endpoint closed");
        assert_eq!(format!("{ports:?}"), "{}", "no slot left behind");
    }

    /// An endpoint whose close polls an acquire of its key, so a close under the
    /// registry's lock deadlocks.
    struct Probe(Arc<Registry<u8, (), Probe>>, Arc<AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            let mut cx = std::task::Context::from_waker(Waker::noop());
            let acquire = pin!(self.0.acquire(0, (), |(): &()| future::pending()));
            assert!(
                acquire.poll(&mut cx).is_pending(),
                "the slot is busy at the close"
            );
            self.1.fetch_add(1, Relaxed);
        }
    }

    #[test]
    fn closes_after_it_releases_the_lock() {
        let ports = Arc::new(Registry::<u8, (), Probe>::new());
        let closes = Arc::new(AtomicUsize::new(0));
        let probe = Probe(Arc::clone(&ports), Arc::clone(&closes));
        let open = |(): &()| future::ready(Ok(probe));
        drop(block_on(ports.acquire(0, (), open)).expect("opens"));
        assert_eq!(closes.load(Relaxed), 1);
        let probe = Probe(Arc::clone(&ports), Arc::clone(&closes));
        let open = |(): &()| future::ready(Ok(probe));
        let mut cx = std::task::Context::from_waker(Waker::noop());
        let Poll::Ready(lease) = pin!(ports.acquire(0, (), open)).poll(&mut cx) else {
            panic!("the slot is free after the close");
        };
        drop(lease.expect("opens again"));
        assert_eq!(closes.load(Relaxed), 2);
    }

    /// A waker that owns a waiting acquire of `tty0`. Its drop drops that acquire,
    /// which takes the registry's lock, so a drop under the lock deadlocks.
    struct Holder {
        _waiter: Mutex<Pending>,
    }

    type Pending = Pin<
        Box<dyn Future<Output = Result<Lease<&'static str, u32, Port>, Error>> + Send>,
    >;

    #[expect(clippy::manual_noop_waker, reason = "its drop is the probe")]
    impl std::task::Wake for Holder {
        fn wake(self: Arc<Self>) {}
    }

    fn holder(ports: &Arc<Ports>) -> Waker {
        let ports = Arc::clone(ports);
        let mut waiter: Pending = Box::pin(async move { busy(&ports).await });
        assert!(poll_with(waiter.as_mut(), &Arc::default()).is_pending());
        Waker::from(Arc::new(Holder {
            _waiter: Mutex::new(waiter),
        }))
    }

    #[test]
    fn drops_a_replaced_or_removed_waker_after_it_releases_the_lock() {
        let ports = Arc::new(Ports::new());
        let mut open = pin!(busy(&ports));
        assert!(poll_with(open.as_mut(), &Arc::default()).is_pending());
        let poll = |waiter: Pin<&mut _>| {
            let waker = holder(&ports);
            Future::poll(waiter, &mut std::task::Context::from_waker(&waker))
        };
        let mut replaced = pin!(busy(&ports));
        assert!(poll(replaced.as_mut()).is_pending());
        assert!(poll_with(replaced.as_mut(), &Arc::default()).is_pending());
        let mut removed = Box::pin(busy(&ports));
        assert!(poll(removed.as_mut()).is_pending());
        drop(removed);
        assert!(poll_with(pin!(busy(&ports)), &Arc::default()).is_pending());
    }

    /// Settings whose compare panics while the registry holds its lock.
    #[derive(Debug)]
    struct Touchy;

    impl PartialEq for Touchy {
        fn eq(&self, _: &Self) -> bool {
            panic!("a panic under the registry lock");
        }
    }

    #[test]
    fn releases_after_a_panic_under_its_lock() {
        let ports = Registry::<&str, Touchy, Port>::new();
        let closes = Arc::new(AtomicUsize::new(0));
        let port = Port {
            n: 0,
            closes: Arc::clone(&closes),
        };
        let lease = block_on(ports.acquire("tty0", Touchy, |_| async { Ok(port) }))
            .expect("opens");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            block_on(ports.acquire("tty0", Touchy, |_| async { unreachable!() }))
        }));
        assert!(unwound.is_err(), "the compare panics");
        drop(lease);
        assert_eq!(closes.load(Relaxed), 1);
        assert_eq!(format!("{ports:?}"), "{}");
    }

    /// An endpoint whose first close panics.
    struct Bomb(bool);

    impl Drop for Bomb {
        fn drop(&mut self) {
            assert!(!self.0, "the close failed");
        }
    }

    #[test]
    fn opens_again_after_a_close_that_panicked() {
        let ports = Registry::<u8, (), Bomb>::new();
        let open = |(): &()| future::ready(Ok(Bomb(true)));
        let lease = block_on(ports.acquire(0, (), open)).expect("opens");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(lease);
        }));
        assert!(unwound.is_err(), "the close panics through the drop");
        let open = |(): &()| future::ready(Ok(Bomb(false)));
        let mut cx = std::task::Context::from_waker(Waker::noop());
        let mut again = std::pin::pin!(ports.acquire(0, (), open));
        let Poll::Ready(again) = again.as_mut().poll(&mut cx) else {
            panic!("the acquire after the close waits forever: {ports:?}");
        };
        drop(again.expect("opens again"));
        assert_eq!(format!("{ports:?}"), "{}");
    }

    /// Settings whose drop panics when its flag is set.
    struct Brittle(bool);

    impl PartialEq for Brittle {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }

    impl Drop for Brittle {
        fn drop(&mut self) {
            assert!(!self.0, "the settings drop failed");
        }
    }

    #[test]
    fn opens_again_after_settings_whose_drop_panicked() {
        let ports = Registry::<u8, Brittle, Port>::new();
        let closes = Arc::new(AtomicUsize::new(0));
        let port = Port {
            n: 0,
            closes: Arc::clone(&closes),
        };
        let lease = block_on(ports.acquire(0, Brittle(true), |_| async { Ok(port) }))
            .expect("opens");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(lease);
        }));
        assert!(
            unwound.is_err(),
            "the settings drop panics through the drop"
        );
        assert_eq!(closes.load(Relaxed), 1);
        let port = Port {
            n: 1,
            closes: Arc::clone(&closes),
        };
        let mut cx = std::task::Context::from_waker(Waker::noop());
        let mut again = pin!(ports.acquire(0, Brittle(false), |_| async { Ok(port) }));
        let Poll::Ready(again) = again.as_mut().poll(&mut cx) else {
            panic!("the acquire after the close waits forever: {ports:?}");
        };
        assert_eq!(again.expect("opens again").n, 1);
    }

    #[test]
    fn shows_its_open_keys() {
        let shown = run(|clock, _, _| async move {
            let ports = Ports::new();
            let opener = Opener::default();
            let open = |_: &u32| opener.open(&clock, ms(5), false);
            let lease = ports.acquire("tty0", 9600, open).await.expect("opens");
            (format!("{ports:?}"), format!("{lease:?}"))
        });
        assert_eq!(
            shown,
            (r#"{"tty0"}"#.into(), r#"Lease { key: "tty0", .. }"#.into())
        );
    }
}

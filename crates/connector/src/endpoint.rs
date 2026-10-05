//! Endpoints that the connectors on one node share.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::future;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Poll, Waker};

use document::diagnostic::{Code, Diagnostic};

use crate::kind::Error;

const SETTINGS: Code = Code::new("connector.endpoint-settings");

/// Keeps at most one open endpoint per key on a node, shared by every connector that
/// names that key. `node` makes one for each kind that needs it and gives it to the
/// kind. Any thread may use it.
pub struct Registry<K, S, T>(Arc<Slots<K, S, T>>);

type Slots<K, S, T> = Mutex<BTreeMap<K, Slot<S, T>>>;

enum Slot<S, T> {
    /// An open or a close runs. The wakers are the acquires that wait for it.
    Busy(Vec<Waker>),
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

impl<K: Ord + Clone + fmt::Debug, S: PartialEq + fmt::Debug, T> Registry<K, S, T> {
    /// Makes an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(BTreeMap::new())))
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
    /// When a thread panicked while it held the registry's lock.
    pub async fn acquire<F>(
        &self,
        key: K,
        settings: S,
        open: impl FnOnce(&S) -> F,
    ) -> Result<Lease<K, S, T>, Error>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let claim = future::poll_fn(|cx| {
            let mut slots = lock(&self.0);
            let slot = match slots.entry(key.clone()) {
                Entry::Vacant(vacant) => {
                    vacant.insert(Slot::Busy(Vec::new()));
                    return Poll::Ready(Ok(Claim::Mine));
                }
                Entry::Occupied(occupied) => occupied.into_mut(),
            };
            match slot {
                Slot::Busy(wakers) => {
                    if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                        wakers.push(cx.waker().clone());
                    }
                    Poll::Pending
                }
                Slot::Open {
                    settings: open,
                    endpoint,
                } => {
                    let endpoint = endpoint
                        .upgrade()
                        .expect("a lease removes its slot before its endpoint drops");
                    if *open == settings {
                        Poll::Ready(Ok(Claim::Shared(endpoint)))
                    } else {
                        Poll::Ready(Err(Error::Config(vec![Diagnostic::new(
                            SETTINGS,
                            None,
                            format!("the endpoint {key:?} is open with other settings: {open:?}"),
                            "Give every connector on this endpoint the same settings".into(),
                        )])))
                    }
                }
            }
        })
        .await?;
        let endpoint = match claim {
            Claim::Shared(endpoint) => endpoint,
            Claim::Mine => {
                let guard = Free {
                    slots: &self.0,
                    key: Some(&key),
                };
                let endpoint = Arc::new(open(&settings).await?);
                let mut slots = lock(&self.0);
                let open = Slot::Open {
                    settings,
                    endpoint: Arc::downgrade(&endpoint),
                };
                let busy = slots.insert(key.clone(), open);
                drop(slots);
                wake(busy);
                guard.disarm();
                endpoint
            }
        };
        Ok(Lease {
            endpoint: Some(endpoint),
            key,
            slots: Arc::clone(&self.0),
        })
    }
}

impl<K: Ord + Clone + fmt::Debug, S: PartialEq + fmt::Debug, T> Default
    for Registry<K, S, T>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug, S, T> fmt::Debug for Registry<K, S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(lock(&self.0).keys()).finish()
    }
}

/// A share of one open endpoint. Derefs to the endpoint. When the last lease on it
/// drops, the endpoint drops (closes) on that thread, and the next
/// [`Registry::acquire`] of its key opens it again after the close. The close runs
/// after the registry's lock is released, so it may be slow or use the registry.
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
    slots: Arc<Slots<K, S, T>>,
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
        let mut slots = lock(&self.slots);
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
        if let Some(slot) = slots.get_mut(&self.key) {
            *slot = Slot::Busy(Vec::new());
        }
        drop(slots);
        drop(endpoint);
        let busy = lock(&self.slots).remove(&self.key);
        wake(busy);
    }
}

impl<K: Ord + fmt::Debug, S, T> fmt::Debug for Lease<K, S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Frees the busy slot of an open that failed or was dropped, and wakes its waiters.
struct Free<'a, K: Ord, S, T> {
    slots: &'a Slots<K, S, T>,
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
            let busy = lock(self.slots).remove(key);
            wake(busy);
        }
    }
}

fn wake<S, T>(slot: Option<Slot<S, T>>) {
    if let Some(Slot::Busy(wakers)) = slot {
        wakers.into_iter().for_each(Waker::wake);
    }
}

fn lock<K, S, T>(slots: &Slots<K, S, T>) -> MutexGuard<'_, BTreeMap<K, Slot<S, T>>> {
    slots.lock().expect("no panic under the registry lock")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
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
                "the endpoint \"tty0\" is open with other settings: 9600".into(),
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

    /// An endpoint that records whether the registry's lock was free at its close.
    struct Probe(Arc<Slots<u8, (), Probe>>, Arc<AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            if self.0.try_lock().is_ok() {
                self.1.fetch_add(1, Relaxed);
            }
        }
    }

    #[test]
    fn closes_after_it_releases_the_lock() {
        let ports = Registry::<u8, (), Probe>::new();
        let free = Arc::new(AtomicUsize::new(0));
        let probe = Probe(Arc::clone(&ports.0), Arc::clone(&free));
        let open = |(): &()| future::ready(Ok(probe));
        drop(block_on(ports.acquire(0, (), open)).expect("opens"));
        assert_eq!(free.load(Relaxed), 1, "the lock was free at the close");
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

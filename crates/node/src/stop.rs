//! A one-time signal that every shard of a node waits on.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[cfg(loom)]
use loom::sync::{Arc, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Arc, Mutex, MutexGuard};

/// Set once, from any thread. Clones share one signal. Each shard touches it once to
/// wait and once to end, never on a frame's path, so a mutex is fine.
#[derive(Clone, Default)]
pub(crate) struct Stop(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    set: bool,
    wakers: Vec<Waker>,
}

impl Stop {
    /// Sets the signal and wakes every guard. Later calls do nothing.
    pub(crate) fn set(&self) {
        let wakers = {
            let mut state = self.lock();
            state.set = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// A shard's hold on the signal: it completes once the signal is set, and it sets
    /// the signal when dropped, so any shard that ends stops the node.
    pub(crate) fn guard(&self) -> Guard {
        Guard {
            stop: self.clone(),
            slot: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .expect("invariant: nothing panics while it holds the stop lock")
    }
}

impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("Stop")
            .field("set", &state.set)
            .field("waiting", &state.wakers.len())
            .finish()
    }
}

/// The future of [`Stop::guard`]. It keeps one waker slot, so a repeated poll does not
/// grow the list.
#[derive(Debug)]
pub(crate) struct Guard {
    stop: Stop,
    slot: Option<usize>,
}

impl Future for Guard {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.stop.lock();
        if state.set {
            return Poll::Ready(());
        }
        if let Some(slot) = self.slot {
            state.wakers[slot].clone_from(cx.waker());
        } else {
            state.wakers.push(cx.waker().clone());
            let slot = state.wakers.len() - 1;
            drop(state);
            self.slot = Some(slot);
        }
        Poll::Pending
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.set();
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;
    use std::task::{Context, Poll, Wake, Waker};

    use super::Stop;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    fn waker() -> (Arc<Count>, Waker) {
        let count = Arc::new(Count::default());
        (Arc::clone(&count), Waker::from(count))
    }

    #[test]
    fn a_repoll_replaces_the_waker_and_set_wakes_only_the_latest() {
        let stop = Stop::default();
        let mut guard = pin!(stop.guard());
        let (a, wa) = waker();
        let (b, wb) = waker();
        let mut ca = Context::from_waker(&wa);
        let mut cb = Context::from_waker(&wb);
        assert_eq!(guard.as_mut().poll(&mut ca), Poll::Pending);
        assert_eq!(guard.as_mut().poll(&mut cb), Poll::Pending);
        assert_eq!(format!("{stop:?}"), "Stop { set: false, waiting: 1 }");
        stop.set();
        assert_eq!(a.0.load(Relaxed), 0);
        assert_eq!(b.0.load(Relaxed), 1);
        assert_eq!(guard.as_mut().poll(&mut cb), Poll::Ready(()));
    }

    #[test]
    fn set_wakes_each_guard_once_and_later_sets_do_nothing() {
        let stop = Stop::default();
        let mut first = pin!(stop.guard());
        let mut second = pin!(stop.guard());
        let (a, wa) = waker();
        let (b, wb) = waker();
        assert_eq!(
            first.as_mut().poll(&mut Context::from_waker(&wa)),
            Poll::Pending
        );
        assert_eq!(
            second.as_mut().poll(&mut Context::from_waker(&wb)),
            Poll::Pending
        );
        stop.set();
        stop.set();
        assert_eq!((a.0.load(Relaxed), b.0.load(Relaxed)), (1, 1));
        assert_eq!(format!("{stop:?}"), "Stop { set: true, waiting: 0 }");
    }

    #[test]
    fn a_dropped_guard_sets_the_signal() {
        let stop = Stop::default();
        let (a, wa) = waker();
        let mut waiting = pin!(stop.guard());
        assert_eq!(
            waiting.as_mut().poll(&mut Context::from_waker(&wa)),
            Poll::Pending
        );
        drop(stop.guard());
        assert_eq!(a.0.load(Relaxed), 1);
        assert_eq!(
            waiting.as_mut().poll(&mut Context::from_waker(&wa)),
            Poll::Ready(())
        );
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use loom::future::block_on;
    use loom::thread;

    use super::Stop;

    /// Two shards wait while a third thread sets the signal; each wait completes.
    #[test]
    fn every_guard_completes_after_a_set_from_another_thread() {
        loom::model(|| {
            let stop = Stop::default();
            let shards: Vec<_> = (0..2)
                .map(|_| {
                    let guard = stop.guard();
                    thread::spawn(move || block_on(guard))
                })
                .collect();
            stop.set();
            for shard in shards {
                shard.join().unwrap();
            }
        });
    }

    /// A guard dropped on one thread wakes a guard that waits on another.
    #[test]
    fn a_guard_dropped_on_one_thread_wakes_another() {
        loom::model(|| {
            let stop = Stop::default();
            let ending = stop.guard();
            let waiting = stop.guard();
            let shard = thread::spawn(move || block_on(waiting));
            thread::spawn(move || drop(ending)).join().unwrap();
            shard.join().unwrap();
        });
    }
}

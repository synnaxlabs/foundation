//! A value given once from one shard to another.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[cfg(loom)]
use loom::sync::{Arc, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Arc, Mutex, MutexGuard};

/// The two ends of one handoff. Each shard touches one once at start, never on a
/// frame's path, so a mutex is fine.
pub(crate) fn pair<T>() -> (Give<T>, Take<T>) {
    let state = Arc::new(Mutex::new(State {
        value: None,
        ended: false,
        waker: None,
    }));
    (Give(Arc::clone(&state)), Take(state))
}

struct State<T> {
    value: Option<T>,
    ended: bool,
    waker: Option<Waker>,
}

fn lock<T>(state: &Mutex<State<T>>) -> MutexGuard<'_, State<T>> {
    state
        .lock()
        .expect("invariant: nothing panics while it holds a handoff lock")
}

/// The giving end. Dropped without [`Give::give`], it ends its [`Take`] with `None`.
pub(crate) struct Give<T>(Arc<Mutex<State<T>>>);

impl<T> Give<T> {
    /// Gives `value` to the [`Take`].
    pub(crate) fn give(self, value: T) {
        lock(&self.0).value = Some(value);
    }
}

impl<T> Drop for Give<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = lock(&self.0);
            state.ended = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The taking end: completes with the value, or with `None` once the [`Give`] is
/// dropped without one.
pub(crate) struct Take<T>(Arc<Mutex<State<T>>>);

impl<T> Future for Take<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = lock(&self.0);
        if state.ended {
            return Poll::Ready(state.value.take());
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> fmt::Debug for Take<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Take")
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

    use super::pair;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    #[test]
    fn a_give_wakes_the_take_once_with_the_value() {
        let (give, take) = pair();
        let mut take = pin!(take);
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(take.as_mut().poll(&mut cx), Poll::Pending);
        give.give(3);
        assert_eq!(count.0.load(Relaxed), 1);
        assert_eq!(take.as_mut().poll(&mut cx), Poll::Ready(Some(3)));
    }

    #[test]
    fn a_dropped_give_ends_the_take_with_none() {
        let (give, take) = pair::<u8>();
        assert_eq!(format!("{take:?}"), "Take");
        let mut take = pin!(take);
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(take.as_mut().poll(&mut cx), Poll::Pending);
        drop(give);
        assert_eq!(count.0.load(Relaxed), 1);
        assert_eq!(take.as_mut().poll(&mut cx), Poll::Ready(None));
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use loom::future::block_on;
    use loom::thread;

    use super::pair;

    /// A value given on one thread reaches a take that waits on another.
    #[test]
    fn a_value_given_on_one_thread_reaches_another() {
        loom::model(|| {
            let (give, take) = pair();
            let shard = thread::spawn(move || block_on(take));
            give.give(7);
            assert_eq!(shard.join().unwrap(), Some(7));
        });
    }

    /// A give dropped on one thread ends a take that waits on another.
    #[test]
    fn a_give_dropped_on_one_thread_ends_another() {
        loom::model(|| {
            let (give, take) = pair::<u8>();
            let shard = thread::spawn(move || block_on(take));
            thread::spawn(move || drop(give)).join().unwrap();
            assert_eq!(shard.join().unwrap(), None);
        });
    }
}

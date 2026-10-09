//! Lists of the wakers of the calls that wait.

use std::task::Waker;

/// Adds `waker` to `wakers` unless one there wakes the same task.
pub(crate) fn register(wakers: &mut Vec<Waker>, waker: &Waker) {
    if !wakers.iter().any(|w| w.will_wake(waker)) {
        wakers.push(waker.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    use super::register;

    #[test]
    fn register_keeps_one_waker_for_each_task() {
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let one = Arc::new(Count(AtomicUsize::new(0)));
        let other = Arc::new(Count(AtomicUsize::new(0)));
        let mut wakers = Vec::new();
        for count in [&one, &one, &other] {
            register(&mut wakers, &Waker::from(Arc::clone(count)));
        }
        wakers.into_iter().for_each(Waker::wake);
        let counts = [&one, &other].map(|count| count.0.load(Ordering::Relaxed));
        assert_eq!(counts, [1, 1]);
    }
}

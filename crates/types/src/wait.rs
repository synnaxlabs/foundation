//! The wakers of futures that wait for one event.

use std::mem;
use std::task::Waker;
use std::vec;

/// The wakers of the futures that wait for one event, by the key of each. A future
/// keeps one waker, the waker of its last poll, and its drop takes it out. Each method
/// that takes a waker out returns it: drop or wake it after the lock or borrow that
/// holds the set ends, because the last drop of a waker can drop a task that takes the
/// same lock.
#[derive(Debug, Default)]
pub struct Set(Vec<(u64, Waker)>);

impl Set {
    /// Makes an empty set. It allocates nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Keeps `waker` for `key` and returns the waker it replaces. A key is unique
    /// among the waiters that can reach the set. It clones `waker` only when the held
    /// waker would not wake the same task, and allocates only when the set grows past
    /// its capacity.
    #[must_use]
    pub fn insert(&mut self, key: u64, waker: &Waker) -> Option<Waker> {
        match self.0.iter_mut().find(|(held, _)| *held == key) {
            Some((_, held)) if held.will_wake(waker) => None,
            Some((_, held)) => Some(mem::replace(held, waker.clone())),
            None => {
                self.0.push((key, waker.clone()));
                None
            }
        }
    }

    /// Takes the waker of `key` out.
    #[must_use]
    pub fn remove(&mut self, key: u64) -> Option<Waker> {
        let at = self.0.iter().position(|(held, _)| *held == key)?;
        Some(self.0.swap_remove(at).1)
    }

    /// Moves each waker into `woken`, and keeps the capacity of the set. It allocates
    /// only when `woken` grows past its capacity.
    pub fn drain(&mut self, woken: &mut Vec<Waker>) {
        woken.extend(self.0.drain(..).map(|(_, waker)| waker));
    }
}

impl IntoIterator for Set {
    type Item = Waker;
    type IntoIter = IntoIter;

    fn into_iter(self) -> IntoIter {
        IntoIter(self.0.into_iter())
    }
}

/// The wakers of a [`Set`], moved out of it.
#[derive(Debug)]
pub struct IntoIter(vec::IntoIter<(u64, Waker)>);

impl Iterator for IntoIter {
    type Item = Waker;

    fn next(&mut self) -> Option<Waker> {
        self.0.next().map(|(_, waker)| waker)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use super::*;

    /// A task that counts its wakes. A waker of it holds a count of its `Arc`.
    #[derive(Default)]
    struct Task(AtomicUsize);

    impl Wake for Task {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn task() -> (Arc<Task>, Waker) {
        let task = Arc::new(Task::default());
        let waker = Waker::from(Arc::clone(&task));
        (task, waker)
    }

    #[test]
    fn an_insert_of_the_same_task_clones_nothing() {
        let (task, waker) = task();
        let mut set = Set::new();
        assert!(set.insert(0, &waker).is_none());
        assert_eq!(Arc::strong_count(&task), 3);
        assert!(set.insert(0, &waker.clone()).is_none());
        assert_eq!(Arc::strong_count(&task), 3, "the set keeps its clone");
    }

    #[test]
    fn an_insert_of_another_task_returns_the_old_waker() {
        let (first, waker) = task();
        let (second, other) = task();
        let mut set = Set::new();
        assert!(set.insert(0, &waker).is_none());
        let old = set.insert(0, &other).expect("replaces");
        assert!(old.will_wake(&waker));
        drop(old);
        assert_eq!(Arc::strong_count(&first), 2, "the set holds no clone of it");
        assert_eq!(Arc::strong_count(&second), 3);
        let mut woken = Vec::new();
        set.drain(&mut woken);
        assert_eq!(woken.len(), 1);
        assert!(woken[0].will_wake(&other));
    }

    #[test]
    fn a_remove_keeps_the_other_keys() {
        let tasks: Vec<(Arc<Task>, Waker)> = (0..3).map(|_| task()).collect();
        let mut set = Set::new();
        for (key, (_, waker)) in (0..).zip(&tasks) {
            assert!(set.insert(key, waker).is_none());
        }
        let removed = set.remove(1).expect("holds key 1");
        assert!(removed.will_wake(&tasks[1].1));
        assert!(set.remove(1).is_none(), "key 1 is gone");
        drop(removed);
        set.into_iter().for_each(Waker::wake);
        let wakes: Vec<usize> = tasks
            .iter()
            .map(|(task, _)| task.0.load(Ordering::Relaxed))
            .collect();
        assert_eq!(wakes, [1, 0, 1]);
    }

    #[test]
    fn a_drain_keeps_the_capacity_of_the_set() {
        let (_task, waker) = task();
        let mut set = Set::new();
        for key in 0..4 {
            assert!(set.insert(key, &waker).is_none());
        }
        let capacity = set.0.capacity();
        let mut woken = Vec::new();
        set.drain(&mut woken);
        assert_eq!((woken.len(), set.0.len()), (4, 0));
        assert_eq!(set.0.capacity(), capacity);
    }

    #[test]
    fn a_new_set_allocates_nothing() {
        assert_eq!(Set::new().0.capacity(), 0);
    }
}

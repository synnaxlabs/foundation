//! Cancellation for a connector's run and its parts.

use std::fmt;
use std::future;
use std::mem;
use std::pin::{Pin, pin};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

/// Cancels a run or one part of it. Clones refer to the same token. Cancelling a
/// token cancels all its children; cancelling a child leaves its parent live. Any
/// thread may use it.
///
/// ```
/// let run = connector::cancel::Token::new();
/// let part = run.child();
/// run.cancel();
/// assert!(part.cancelled(), "a child cancels with its parent");
/// ```
#[derive(Clone)]
pub struct Token(Arc<Node>);

impl Token {
    /// Makes a live root token. The supervisor makes one for each run. A kind makes
    /// its parts with [`Token::child`]; a token from `new` cancels only through
    /// itself.
    #[must_use]
    #[expect(
        clippy::new_without_default,
        reason = "a defaulted field would make a root that the run's cancel never reaches"
    )]
    pub fn new() -> Self {
        Self(Arc::new(Node {
            parent: None,
            cancelled: AtomicBool::new(false),
            state: Mutex::default(),
        }))
    }

    /// Makes a child that cancels when this token cancels. A child of a cancelled
    /// token starts cancelled.
    #[must_use]
    pub fn child(&self) -> Self {
        let mut parent = self.0.lock();
        if self.0.cancelled.load(Relaxed) {
            return Self(Arc::new(Node {
                parent: None,
                cancelled: AtomicBool::new(true),
                state: Mutex::default(),
            }));
        }
        Self(Arc::new_cyclic(|me| Node {
            parent: Some((
                Arc::clone(&self.0),
                parent.children.insert(Weak::clone(me)),
            )),
            cancelled: AtomicBool::new(false),
            state: Mutex::default(),
        }))
    }

    /// Cancels this token and its children: wakes every [`Wait`], then runs each hook
    /// on the calling thread. On return, every token below this one is cancelled.
    /// When another call cancels some of them at the same time, that call runs
    /// their hooks, and they may not have run yet.
    ///
    /// # Panics
    ///
    /// When a hook panics. The tokens are cancelled, but the hooks after it do not
    /// run, so the parts they unblock can stay blocked. A hook must not panic.
    pub fn cancel(&self) {
        let mut hooks = Vec::new();
        let mut nodes = vec![Arc::clone(&self.0)];
        while let Some(node) = nodes.pop() {
            // A node another call flagged is still walked, since that call may not
            // have reached its children yet. Children stay listed for this walk.
            let (wakers, taken, children) = {
                let mut state = node.lock();
                let children: Vec<_> = state.children.values().cloned().collect();
                if node.cancelled.load(Relaxed) {
                    (Slab::default(), Slab::default(), children)
                } else {
                    node.cancelled.store(true, Release);
                    (
                        mem::take(&mut state.wakers),
                        mem::take(&mut state.hooks),
                        children,
                    )
                }
            };
            wakers.into_values().for_each(Waker::wake);
            hooks.extend(taken.into_values());
            nodes.extend(children.iter().filter_map(Weak::upgrade));
        }
        for hook in hooks {
            hook();
        }
    }

    /// Whether the token is cancelled.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.0.cancelled.load(Acquire)
    }

    /// Returns a future that completes when the token is cancelled. It is safe to
    /// drop at any time. After its first poll, polling allocates nothing.
    ///
    /// ```
    /// async fn read(cancel: &connector::cancel::Token) {
    ///     cancel.wait().await;
    /// }
    /// ```
    pub fn wait(&self) -> Wait {
        Wait {
            node: Arc::clone(&self.0),
            slot: None,
        }
    }

    /// Runs `f` until it completes or this token is cancelled. Returns `None` when the
    /// token is cancelled before a poll of `f`, also when it already is, so a cancel
    /// wins over an `f` that is ready at the same time. A cancel during a poll of `f`
    /// does not stop that poll, and `race` returns its output; check `cancelled`
    /// after `race` when that matters. On `None`, `f` is dropped, and what it did
    /// before the cancel stays done.
    /// The first poll may allocate a slot in the token; later polls allocate
    /// nothing, and a poll with the same waker as the last one takes no lock.
    ///
    /// ```
    /// async fn read(cancel: &connector::cancel::Token) -> Option<u8> {
    ///     cancel.race(async { 7 }).await
    /// }
    /// ```
    pub async fn race<F: Future>(&self, f: F) -> Option<F::Output> {
        let mut f = pin!(f);
        let mut wait = pin!(self.wait());
        future::poll_fn(|cx| {
            if wait.as_mut().poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            f.as_mut().poll(cx).map(Some)
        })
        .await
    }

    /// Runs `f` once, on the thread that cancels, to unblock a blocking vendor call.
    /// That thread may be a shard, so `f` must not block. When the token is already
    /// cancelled, runs `f` now.
    ///
    /// `f` can run before the vendor call starts, so the call would then block
    /// forever. Make `f` close or abort the handle so that later calls also return,
    /// or check [`Token::cancelled`] before each blocking call.
    ///
    /// Dropping the returned [`Hook`] before cancel removes `f`, but does not wait
    /// for an `f` that already runs. So `f` must hold an `Arc` to any vendor handle
    /// it uses, and the handle must close only when the last `Arc` drops.
    ///
    /// # Panics
    ///
    /// When the token is already cancelled and `f` panics.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let cancel = connector::cancel::Token::new();
    /// let stopped = Arc::new(AtomicBool::new(false));
    /// let flag = Arc::clone(&stopped);
    /// let _hook = cancel.hook(move || flag.store(true, Ordering::Relaxed));
    /// cancel.cancel();
    /// assert!(stopped.load(Ordering::Relaxed), "the hook ran");
    /// ```
    pub fn hook(&self, f: impl FnOnce() + Send + 'static) -> Hook {
        let mut state = self.0.lock();
        if self.0.cancelled.load(Relaxed) {
            drop(state);
            f();
            return Hook { node: None };
        }
        let key = state.hooks.insert(Box::new(f));
        Hook {
            node: Some((Arc::clone(&self.0), key)),
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("cancelled", &self.cancelled())
            .finish_non_exhaustive()
    }
}

/// The future from [`Token::wait`]. It holds its own handle to the token, so it can
/// move to another task. It completes at once when the token is already cancelled,
/// and never when every token that could cancel it drops first.
#[must_use = "a future does nothing unless it is awaited"]
pub struct Wait {
    node: Arc<Node>,
    /// The waker's slot in `node` and a copy of that waker, from the first poll on.
    slot: Option<(usize, Waker)>,
}

impl Future for Wait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        if this.node.cancelled.load(Acquire) {
            this.slot = None;
            return Poll::Ready(());
        }
        if let Some((_, last)) = &this.slot
            && last.will_wake(cx.waker())
        {
            return Poll::Pending;
        }
        // Waker clones and drops run executor code, so they happen outside the lock.
        let waker = cx.waker().clone();
        let mut state = this.node.lock();
        if this.node.cancelled.load(Relaxed) {
            drop(state);
            this.slot = None;
            return Poll::Ready(());
        }
        let mut old = None;
        if let Some((key, last)) = this.slot.take() {
            old = Some((mem::replace(state.wakers.get_mut(key), waker.clone()), last));
            this.slot = Some((key, waker));
        } else {
            this.slot = Some((state.wakers.insert(waker.clone()), waker));
        }
        drop(state);
        drop(old);
        Poll::Pending
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if let Some((key, _)) = &self.slot {
            drop(self.node.remove(*key, |state| &mut state.wakers));
        }
    }
}

impl fmt::Debug for Wait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wait")
            .field("cancelled", &self.node.cancelled.load(Acquire))
            .finish_non_exhaustive()
    }
}

/// Keeps a hook from [`Token::hook`] registered. Drop it to remove the hook.
#[must_use = "dropping a hook removes it"]
pub struct Hook {
    /// `None` when the hook already ran in [`Token::hook`].
    node: Option<(Arc<Node>, usize)>,
}

impl Drop for Hook {
    fn drop(&mut self) {
        if let Some((node, key)) = &self.node {
            drop(node.remove(*key, |state| &mut state.hooks));
        }
    }
}

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hook").finish_non_exhaustive()
    }
}

/// One token in the tree. A child holds its parent; a parent holds only weak links.
/// A child leaves a live parent's list at its last drop.
struct Node {
    /// The parent and this node's slot in its children.
    parent: Option<(Arc<Node>, usize)>,
    /// Set once, under `state`'s lock. After it is set, nothing is inserted into or
    /// removed from `state`, so a stale slot key never removes another entry.
    cancelled: AtomicBool,
    state: Mutex<State>,
}

impl Node {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("invariant: nothing panics while it holds a token's lock")
    }

    /// Takes the entry at `key` out of a slab, unless the token is cancelled. The
    /// caller drops it after the lock is released, since its drop may run any code.
    fn remove<T>(
        &self,
        key: usize,
        slab: impl FnOnce(&mut State) -> &mut Slab<T>,
    ) -> Option<T> {
        let mut state = self.lock();
        if self.cancelled.load(Relaxed) {
            return None;
        }
        Some(slab(&mut state).remove(key))
    }
}

impl Drop for Node {
    /// Walks up in a loop, not by recursion, so a long chain of children does not
    /// overflow the stack when its last token drops.
    fn drop(&mut self) {
        let mut link = self.parent.take();
        while let Some((parent, key)) = link {
            drop(parent.remove(key, |state| &mut state.children));
            let Some(mut parent) = Arc::into_inner(parent) else {
                return;
            };
            link = parent.parent.take();
        }
    }
}

#[derive(Default)]
struct State {
    wakers: Slab<Waker>,
    hooks: Slab<Box<dyn FnOnce() + Send>>,
    children: Slab<Weak<Node>>,
}

/// Values in reused slots, so a waiter, hook, or child that comes and goes does not
/// grow its token.
struct Slab<T> {
    slots: Vec<Option<T>>,
    free: Vec<usize>,
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T> Slab<T> {
    fn insert(&mut self, value: T) -> usize {
        if let Some(key) = self.free.pop() {
            self.slots[key] = Some(value);
            key
        } else {
            self.slots.push(Some(value));
            self.slots.len() - 1
        }
    }

    fn get_mut(&mut self, key: usize) -> &mut T {
        self.slots[key]
            .as_mut()
            .expect("invariant: a live key names a full slot")
    }

    fn remove(&mut self, key: usize) -> T {
        let value = self.slots[key]
            .take()
            .expect("invariant: a live key names a full slot");
        self.free.push(key);
        value
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().flatten()
    }

    fn into_values(self) -> impl Iterator<Item = T> {
        self.slots.into_iter().flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::SeqCst;
    use std::task::Wake;

    use proptest::prelude::*;

    use super::*;

    /// A waker that counts its wakes.
    #[derive(Default)]
    struct Tally(AtomicU64);

    impl Wake for Tally {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, SeqCst);
        }
    }

    impl Tally {
        fn waker() -> (Arc<Self>, Waker) {
            let tally = Arc::new(Self::default());
            let waker = Waker::from(Arc::clone(&tally));
            (tally, waker)
        }

        fn wakes(&self) -> u64 {
            self.0.load(SeqCst)
        }
    }

    fn poll<F: Future>(f: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        f.poll(&mut Context::from_waker(waker))
    }

    /// Counts how often a hook runs.
    fn counter() -> (Arc<AtomicU64>, impl FnOnce() + Send + 'static) {
        let runs = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&runs);
        (runs, move || {
            shared.fetch_add(1, SeqCst);
        })
    }

    fn entries(token: &Token) -> (usize, usize, usize) {
        fn live<T>(slab: &Slab<T>) -> usize {
            slab.values().count()
        }
        let state = token.0.lock();
        (
            live(&state.wakers),
            live(&state.hooks),
            live(&state.children),
        )
    }

    mod cancel {
        use super::*;

        #[test]
        fn wakes_every_waiter() {
            let token = Token::new();
            let tallies: Vec<_> = (0..3).map(|_| Tally::waker()).collect();
            let mut waits: Vec<_> =
                tallies.iter().map(|_| Box::pin(token.wait())).collect();
            for (wait, (_, waker)) in waits.iter_mut().zip(&tallies) {
                assert_eq!(poll(wait.as_mut(), waker), Poll::Pending, "live token");
            }
            token.cancel();
            for (wait, (tally, waker)) in waits.iter_mut().zip(&tallies) {
                assert_eq!(tally.wakes(), 1, "cancel wakes each waiter once");
                assert_eq!(poll(wait.as_mut(), waker), Poll::Ready(()), "cancelled");
            }
        }

        /// A waker that cancels its token and records whether the grandchild was
        /// cancelled when that call returned.
        struct Recancel(Token, Token, AtomicBool);

        impl Wake for Recancel {
            fn wake(self: Arc<Self>) {
                self.0.cancel();
                self.2.store(self.1.cancelled(), SeqCst);
            }
        }

        #[test]
        fn returns_after_its_children_are_cancelled_during_another_cancel() {
            let root = Token::new();
            let child = root.child();
            let grandchild = child.child();
            let check =
                Arc::new(Recancel(child.clone(), grandchild, AtomicBool::new(false)));
            let mut wait = pin!(child.wait());
            let waker = Waker::from(Arc::clone(&check));
            assert_eq!(poll(wait.as_mut(), &waker), Poll::Pending, "live token");
            root.cancel();
            assert!(check.2.load(SeqCst), "the grandchild was cancelled");
        }

        #[test]
        fn never_loses_the_wake_of_a_concurrent_poll() {
            for round in 0..20_000 {
                let token = Token::new();
                let (tally, waker) = Tally::waker();
                let mut wait = Box::pin(token.wait());
                let started = AtomicBool::new(false);
                let first = std::thread::scope(|scope| {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "a test owns its threads"
                    )]
                    scope.spawn(|| {
                        started.store(true, SeqCst);
                        token.cancel();
                    });
                    while !started.load(SeqCst) {
                        std::hint::spin_loop();
                    }
                    poll(wait.as_mut(), &waker)
                });
                if first.is_pending() {
                    assert_eq!(tally.wakes(), 1, "round {round} lost its wake");
                }
            }
        }

        #[test]
        fn twice_does_nothing_more() {
            let token = Token::new();
            let (runs, f) = counter();
            let _hook = token.hook(f);
            token.cancel();
            token.cancel();
            assert!(token.cancelled(), "the token is cancelled");
            assert_eq!(runs.load(SeqCst), 1, "the hook runs once");
        }

        #[test]
        fn wakes_waiters_of_children_before_hooks_run() {
            let token = Token::new();
            let child = token.child();
            let (tally, waker) = Tally::waker();
            let mut wait = pin!(child.wait());
            assert_eq!(poll(wait.as_mut(), &waker), Poll::Pending, "live token");
            let seen = Arc::new(AtomicU64::new(u64::MAX));
            let (shared, observed) = (Arc::clone(&tally), Arc::clone(&seen));
            let _hook = token.hook(move || observed.store(shared.wakes(), SeqCst));
            token.cancel();
            assert_eq!(seen.load(SeqCst), 1, "the child's waiter woke first");
        }
    }

    #[test]
    fn debug_shows_whether_cancelled() {
        let token = Token::new();
        let wait = token.wait();
        let hook = token.hook(|| {});
        assert_eq!(
            format!("{token:?}"),
            "Token { cancelled: false, .. }",
            "live"
        );
        assert_eq!(format!("{wait:?}"), "Wait { cancelled: false, .. }", "live");
        assert_eq!(format!("{hook:?}"), "Hook { .. }", "a hook");
        token.cancel();
        assert_eq!(
            format!("{token:?}"),
            "Token { cancelled: true, .. }",
            "done"
        );
        assert_eq!(format!("{wait:?}"), "Wait { cancelled: true, .. }", "done");
    }

    mod child {
        use super::*;

        #[test]
        fn cancels_with_its_parent() {
            let parent = Token::new();
            let grandchild = parent.child().child();
            parent.cancel();
            assert!(grandchild.cancelled(), "a grandchild cancels with its root");
        }

        #[test]
        fn leaves_its_parent_live() {
            let parent = Token::new();
            let child = parent.child();
            child.cancel();
            assert!(child.cancelled(), "the child is cancelled");
            assert!(!parent.cancelled(), "the parent stays live");
            assert_eq!(entries(&parent), (0, 0, 1), "a held child stays listed");
            drop(child);
            assert_eq!(entries(&parent), (0, 0, 0), "the parent forgot the child");
        }

        #[test]
        fn of_a_cancelled_token_starts_cancelled() {
            let parent = Token::new();
            parent.cancel();
            assert!(parent.child().cancelled(), "born cancelled");
        }

        #[test]
        fn dropped_leaves_no_entry_in_its_parent() {
            let parent = Token::new();
            for _ in 0..100 {
                drop(parent.child().child());
            }
            assert_eq!(entries(&parent), (0, 0, 0), "no child is left");
            assert!(
                parent.0.lock().children.slots.len() <= 1,
                "slots are reused"
            );
        }

        #[test]
        fn a_deep_chain_drops_without_overflowing_the_stack() {
            let root = Token::new();
            let mut leaf = root.child();
            for _ in 0..200_000 {
                leaf = leaf.child();
            }
            drop(leaf);
            assert!(!root.cancelled(), "the root stays live");
            assert_eq!(entries(&root), (0, 0, 0), "the chain left the root");
        }

        #[test]
        fn cancels_through_a_dropped_middle_token() {
            let root = Token::new();
            let leaf = root.child().child();
            root.cancel();
            assert!(leaf.cancelled(), "the leaf keeps its dropped parent alive");
        }
    }

    mod race {
        use super::*;

        /// A future that counts its polls and drops, and completes when `ready`.
        struct Probe {
            ready: Arc<AtomicBool>,
            polls: Arc<AtomicU64>,
            drops: Arc<AtomicU64>,
        }

        impl Future for Probe {
            type Output = u8;

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u8> {
                self.polls.fetch_add(1, SeqCst);
                if self.ready.load(SeqCst) {
                    Poll::Ready(7)
                } else {
                    Poll::Pending
                }
            }
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                self.drops.fetch_add(1, SeqCst);
            }
        }

        fn probe(ready: bool) -> (Probe, Arc<AtomicU64>, Arc<AtomicU64>) {
            let polls = Arc::new(AtomicU64::new(0));
            let drops = Arc::new(AtomicU64::new(0));
            let f = Probe {
                ready: Arc::new(AtomicBool::new(ready)),
                polls: Arc::clone(&polls),
                drops: Arc::clone(&drops),
            };
            (f, polls, drops)
        }

        #[test]
        fn returns_the_output_when_the_future_completes_first() {
            let token = Token::new();
            let (f, _, _) = probe(true);
            let (_, waker) = Tally::waker();
            let race = pin!(token.race(f));
            assert_eq!(poll(race, &waker), Poll::Ready(Some(7)), "f won");
        }

        #[test]
        fn returns_none_and_drops_the_future_when_cancelled_during_the_wait() {
            let token = Token::new();
            let (f, polls, drops) = probe(false);
            let (tally, waker) = Tally::waker();
            let mut race = pin!(token.race(f));
            assert_eq!(poll(race.as_mut(), &waker), Poll::Pending, "both wait");
            token.cancel();
            assert_eq!(tally.wakes(), 1, "cancel wakes the race");
            assert_eq!(poll(race.as_mut(), &waker), Poll::Ready(None), "cancelled");
            assert_eq!(polls.load(SeqCst), 1, "f is not polled after cancel");
            assert_eq!(drops.load(SeqCst), 1, "f is dropped on return");
        }

        #[test]
        fn never_polls_the_future_when_already_cancelled() {
            let token = Token::new();
            token.cancel();
            let (f, polls, _) = probe(true);
            let (_, waker) = Tally::waker();
            let race = pin!(token.race(f));
            assert_eq!(poll(race, &waker), Poll::Ready(None), "cancelled");
            assert_eq!(polls.load(SeqCst), 0, "f is never polled");
        }

        #[test]
        fn returns_none_when_both_are_ready_on_one_poll() {
            let token = Token::new();
            let (f, polls, _) = probe(false);
            let ready = Arc::clone(&f.ready);
            let (_, waker) = Tally::waker();
            let mut race = pin!(token.race(f));
            assert_eq!(poll(race.as_mut(), &waker), Poll::Pending, "both wait");
            ready.store(true, SeqCst);
            token.cancel();
            assert_eq!(poll(race, &waker), Poll::Ready(None), "cancel wins");
            assert_eq!(polls.load(SeqCst), 1, "f is not polled again");
        }

        /// Cancels the token inside its own poll, then completes.
        struct Cancels(Token);

        impl Future for Cancels {
            type Output = u8;

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u8> {
                self.0.cancel();
                Poll::Ready(7)
            }
        }

        #[test]
        fn returns_the_output_when_cancelled_during_the_poll_of_the_future() {
            let token = Token::new();
            let (_, waker) = Tally::waker();
            let race = pin!(token.race(Cancels(token.clone())));
            assert_eq!(poll(race, &waker), Poll::Ready(Some(7)), "the poll ran");
            assert!(token.cancelled(), "the token is cancelled");
        }

        #[test]
        fn wakes_the_waker_of_the_last_poll() {
            let token = Token::new();
            let (f, _, _) = probe(false);
            let (first, a) = Tally::waker();
            let (second, b) = Tally::waker();
            let mut race = pin!(token.race(f));
            assert_eq!(poll(race.as_mut(), &a), Poll::Pending, "both wait");
            assert_eq!(poll(race.as_mut(), &b), Poll::Pending, "both wait");
            token.cancel();
            assert_eq!((first.wakes(), second.wakes()), (0, 1), "the newest waker");
            assert_eq!(poll(race.as_mut(), &b), Poll::Ready(None), "cancelled");
        }

        #[test]
        fn leaves_no_waker_when_dropped() {
            let token = Token::new();
            let (f, _, _) = probe(false);
            let (_, waker) = Tally::waker();
            let mut race = Box::pin(token.race(f));
            assert_eq!(poll(race.as_mut(), &waker), Poll::Pending, "both wait");
            assert_eq!(entries(&token), (1, 0, 0), "the race waits");
            drop(race);
            assert_eq!(entries(&token), (0, 0, 0), "the race left");
        }
    }

    mod wait {
        use super::*;

        #[test]
        fn is_ready_at_once_when_cancelled() {
            let token = Token::new();
            token.cancel();
            let (tally, waker) = Tally::waker();
            assert_eq!(
                poll(pin!(token.wait()), &waker),
                Poll::Ready(()),
                "cancelled"
            );
            assert_eq!(tally.wakes(), 0, "no wake is needed");
        }

        #[test]
        fn dropped_leaves_no_waker() {
            let token = Token::new();
            let (_, waker) = Tally::waker();
            for _ in 0..100 {
                let mut wait = Box::pin(token.wait());
                assert_eq!(poll(wait.as_mut(), &waker), Poll::Pending, "live token");
            }
            assert_eq!(entries(&token), (0, 0, 0), "no waker is left");
            assert!(token.0.lock().wakers.slots.len() <= 1, "slots are reused");
        }

        /// A waker whose last drop drops a wait on the same token, as a task's
        /// future does when an executor frees the task with its last waker.
        struct Owner(Mutex<Option<Pin<Box<Wait>>>>);

        impl Wake for Owner {
            fn wake(self: Arc<Self>) {
                let wait = self.0.lock().expect("no panic under this lock").take();
                drop(wait);
            }
        }

        #[test]
        fn drops_a_replaced_waker_outside_the_lock() {
            let token = Token::new();
            let (_, plain) = Tally::waker();
            let mut inner = Box::pin(token.wait());
            assert_eq!(poll(inner.as_mut(), &plain), Poll::Pending, "live token");
            let owner = Waker::from(Arc::new(Owner(Mutex::new(Some(inner)))));
            let mut outer = pin!(token.wait());
            assert_eq!(poll(outer.as_mut(), &owner), Poll::Pending, "live token");
            drop(owner);
            assert_eq!(poll(outer.as_mut(), &plain), Poll::Pending, "no deadlock");
            assert_eq!(entries(&token), (1, 0, 0), "the inner wait left");
        }

        #[test]
        fn takes_no_lock_when_polled_again_with_the_same_waker() {
            let token = Token::new();
            let (_, waker) = Tally::waker();
            let mut wait = pin!(token.wait());
            assert_eq!(poll(wait.as_mut(), &waker), Poll::Pending, "live token");
            let state = token.0.lock();
            assert_eq!(poll(wait.as_mut(), &waker), Poll::Pending, "no deadlock");
            drop(state);
        }

        #[test]
        fn wakes_the_waker_of_the_last_poll() {
            let token = Token::new();
            let (first, a) = Tally::waker();
            let (second, b) = Tally::waker();
            let mut wait = pin!(token.wait());
            assert_eq!(poll(wait.as_mut(), &a), Poll::Pending, "live token");
            assert_eq!(poll(wait.as_mut(), &b), Poll::Pending, "live token");
            assert_eq!(entries(&token), (1, 0, 0), "one waker per wait");
            token.cancel();
            assert_eq!((first.wakes(), second.wakes()), (0, 1), "the newest waker");
        }
    }

    mod hook {
        use super::*;

        #[test]
        fn runs_once_before_cancel_returns() {
            let token = Token::new();
            let (runs, f) = counter();
            let _hook = token.hook(f);
            assert_eq!(runs.load(SeqCst), 0, "not before cancel");
            token.cancel();
            assert_eq!(runs.load(SeqCst), 1, "the hook ran in cancel");
        }

        #[test]
        fn runs_at_once_when_cancelled() {
            let token = Token::new();
            token.cancel();
            let (runs, f) = counter();
            let hook = token.hook(f);
            assert_eq!(runs.load(SeqCst), 1, "a late hook runs at once");
            drop(hook);
            assert_eq!(runs.load(SeqCst), 1, "dropping it changes nothing");
        }

        #[test]
        fn dropped_never_runs() {
            let token = Token::new();
            let (runs, f) = counter();
            drop(token.hook(f));
            assert_eq!(entries(&token), (0, 0, 0), "the hook is removed");
            token.cancel();
            assert_eq!(runs.load(SeqCst), 0, "a dropped hook never runs");
        }

        #[test]
        fn may_cancel_tokens_and_add_hooks() {
            let token = Token::new();
            let other = Token::new();
            let (runs, f) = counter();
            let (inner, again) = (other.clone(), token.clone());
            let _hook = token.hook(move || {
                inner.cancel();
                again.cancel();
                drop(again.hook(f));
            });
            token.cancel();
            assert!(other.cancelled(), "the hook cancelled another token");
            assert_eq!(runs.load(SeqCst), 1, "a hook added in a hook runs at once");
        }
    }

    /// One step of a script over a tree of tokens.
    #[derive(Clone, Debug)]
    enum Op {
        Child(usize),
        Cancel(usize),
        Drop(usize),
        Hook(usize),
        DropHook(usize),
        Wait(usize),
        DropWait(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        let i = 0..16_usize;
        prop_oneof![
            i.clone().prop_map(Op::Child),
            i.clone().prop_map(Op::Cancel),
            i.clone().prop_map(Op::Drop),
            i.clone().prop_map(Op::Hook),
            i.clone().prop_map(Op::DropHook),
            i.clone().prop_map(Op::Wait),
            i.prop_map(Op::DropWait),
        ]
    }

    /// The model of one token: its parent, and whether it is cancelled.
    struct Model {
        parent: Option<usize>,
        cancelled: bool,
    }

    fn cancelled(model: &[Model], mut node: usize) -> bool {
        loop {
            if model[node].cancelled {
                return true;
            }
            match model[node].parent {
                Some(parent) => node = parent,
                None => return false,
            }
        }
    }

    /// Node, guard, and run count of a hook, and the count when the guard dropped.
    type Hooked = (usize, Option<Hook>, Arc<AtomicU64>, Option<u64>);

    /// Node, future, waker, and whether the first poll was ready, of a wait.
    type Waiting = (usize, Option<Pin<Box<Wait>>>, Arc<Tally>, Waker, bool);

    struct Run {
        model: Vec<Model>,
        tokens: Vec<Option<Token>>,
        hooks: Vec<Hooked>,
        waits: Vec<Waiting>,
    }

    impl Run {
        fn new() -> Self {
            Self {
                model: vec![Model {
                    parent: None,
                    cancelled: false,
                }],
                tokens: vec![Some(Token::new())],
                hooks: Vec::new(),
                waits: Vec::new(),
            }
        }

        fn cancelled(&self, node: usize) -> bool {
            cancelled(&self.model, node)
        }

        fn apply(&mut self, op: &Op) {
            let pick = |len: usize, i: usize| i % len;
            match *op {
                Op::Child(i) => {
                    let i = pick(self.tokens.len(), i);
                    if let Some(child) = self.tokens[i].as_ref().map(Token::child) {
                        self.model.push(Model {
                            parent: Some(i),
                            cancelled: false,
                        });
                        self.tokens.push(Some(child));
                    }
                }
                Op::Cancel(i) => {
                    let i = pick(self.tokens.len(), i);
                    if let Some(token) = &self.tokens[i] {
                        token.cancel();
                        self.model[i].cancelled = true;
                    }
                }
                Op::Drop(i) => {
                    let i = pick(self.tokens.len(), i);
                    self.tokens[i] = None;
                }
                Op::Hook(i) => {
                    let i = pick(self.tokens.len(), i);
                    if let Some(token) = &self.tokens[i] {
                        let (runs, f) = counter();
                        self.hooks.push((i, Some(token.hook(f)), runs, None));
                    }
                }
                Op::DropHook(i) if !self.hooks.is_empty() => {
                    let i = pick(self.hooks.len(), i);
                    let hook = &mut self.hooks[i];
                    if hook.1.take().is_some() {
                        hook.3 = Some(hook.2.load(SeqCst));
                    }
                }
                Op::Wait(i) => {
                    let i = pick(self.tokens.len(), i);
                    if let Some(token) = &self.tokens[i] {
                        let (tally, waker) = Tally::waker();
                        let mut wait = Box::pin(token.wait());
                        let ready = poll(wait.as_mut(), &waker).is_ready();
                        assert_eq!(ready, self.cancelled(i), "first poll");
                        self.waits.push((i, Some(wait), tally, waker, ready));
                    }
                }
                Op::DropWait(i) if !self.waits.is_empty() => {
                    let i = pick(self.waits.len(), i);
                    self.waits[i].1 = None;
                }
                Op::DropHook(_) | Op::DropWait(_) => {}
            }
        }

        fn check(&mut self) {
            for (i, token) in self.tokens.iter().enumerate() {
                if let Some(token) = token {
                    assert_eq!(token.cancelled(), self.cancelled(i), "token {i}");
                }
            }
            for (node, _, runs, frozen) in &self.hooks {
                let expected = frozen.unwrap_or(u64::from(self.cancelled(*node)));
                assert_eq!(runs.load(SeqCst), expected, "hook on token {node}");
            }
            for (node, wait, tally, waker, ready) in &mut self.waits {
                let cancelled = cancelled(&self.model, *node);
                if let Some(wait) = wait {
                    let woken = cancelled && !*ready;
                    assert_eq!(tally.wakes(), u64::from(woken), "wake on token {node}");
                    let ready = poll(wait.as_mut(), waker).is_ready();
                    assert_eq!(ready, cancelled, "poll on token {node}");
                }
            }
        }
    }

    proptest! {
        #[test]
        fn matches_the_model(ops in prop::collection::vec(op(), 1..64)) {
            let mut run = Run::new();
            for op in &ops {
                run.apply(op);
                run.check();
            }
        }
    }
}

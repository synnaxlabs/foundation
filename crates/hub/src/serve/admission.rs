//! The hello that a session admitted, its renewals, and its end.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use access::proof::{self, Admitted};
use types::ed25519::PublicKey;
use types::name::Name;
use types::time::Stamp;
use types::wait;
use wire::hub::client::Signed;

use super::Error;
use crate::State;

/// The hello that a session admitted last, and its signature. The first
/// [`Admission::admit`] admits a hello, and each later one renews it.
#[derive(Debug)]
pub(crate) struct Admission {
    /// The public key of the node that carried each hello of the session.
    peer: PublicKey,
    admitted: RefCell<Option<(Admitted, [u8; 64])>>,
    /// The count of hellos admitted.
    count: Cell<u64>,
    /// The waker of each [`Renewal`] that waits, by its key.
    waiting: RefCell<wait::Set>,
    /// The key of the next [`Renewal`].
    key: Cell<u64>,
}

impl Admission {
    /// An admission of the hellos that `peer` carries: this node for a client
    /// session.
    pub(crate) fn new(peer: PublicKey) -> Self {
        Self {
            peer,
            admitted: RefCell::default(),
            count: Cell::default(),
            waiting: RefCell::default(),
            key: Cell::default(),
        }
    }

    /// Admits `signed`, or renews the hello that the session holds with it. A renewal
    /// keeps the peer of the first hello. Checks no nonce: the node that carried the
    /// hello checks it.
    ///
    /// # Errors
    ///
    /// [`Error::Access`] when the rules refuse the hello.
    pub(crate) fn admit(
        &self,
        state: &RefCell<State>,
        signed: Signed,
    ) -> Result<(), Error> {
        let Signed { hello, signature } = signed;
        {
            let state = state.borrow();
            let now = state.time.now().mesh;
            let mut admitted = self.admitted.borrow_mut();
            let next = match admitted.as_ref() {
                None => state.rules.admit(now, self.peer, hello, &signature),
                Some((first, _)) => state.rules.renew(first, now, hello, &signature),
            }
            .map_err(|error| Error::Access(Box::new(error)))?;
            *admitted = Some((next, signature));
        }
        self.wake();
        Ok(())
    }

    /// Counts a hello admitted, and wakes each [`Renewal`] that waits. The count moves
    /// only here, with the drain, so a renewal reads an unchanged count as its waker
    /// still in the set.
    fn wake(&self) {
        self.count.set(self.count.get() + 1);
        // A wake can drop a task whose renewal borrows the map.
        let mut woken = Vec::new();
        self.waiting.borrow_mut().drain(&mut woken);
        for waker in woken {
            waker.wake();
        }
    }

    /// The admitted hello and its signature, with the count of hellos admitted
    /// through it.
    ///
    /// # Panics
    ///
    /// When no hello is admitted.
    fn signed(&self) -> (Signed, u64) {
        let admitted = self.admitted.borrow();
        let (admitted, signature) = admitted
            .as_ref()
            .expect("invariant: a session holds a hello only after its admission");
        let signed = Signed {
            hello: admitted.hello().clone(),
            signature: *signature,
        };
        (signed, self.count.get())
    }

    /// A wait for the renewals of the session, from now until its drop.
    pub(crate) fn renewal(&self) -> Renewal<'_> {
        let key = self.key.get();
        self.key.set(key + 1);
        Renewal {
            admission: self,
            key,
            seen: Cell::new(self.count.get()),
            held: Cell::new(None),
        }
    }

    /// The subject of the admitted hello, or `None` before one. A renewal never
    /// changes the subject.
    pub(crate) fn subject(&self) -> Option<Name> {
        self.admitted
            .borrow()
            .as_ref()
            .map(|(admitted, _)| admitted.hello().subject.clone())
    }

    /// Verifies `signature`, of `kind`, over `bytes` with the admitted hello, and
    /// gives that hello and its signature.
    ///
    /// # Errors
    ///
    /// [`Error::Access`] when the rules refuse the signature.
    ///
    /// # Panics
    ///
    /// When no hello is admitted.
    pub(crate) fn verify(
        &self,
        state: &RefCell<State>,
        kind: proof::Kind,
        bytes: &[u8],
        signature: &[u8; 64],
    ) -> Result<Signed, Error> {
        let state = state.borrow();
        let admitted = self.admitted.borrow();
        let (admitted, kept) = admitted
            .as_ref()
            .expect("invariant: a session verifies only after its hello");
        state
            .rules
            .verify(admitted, state.time.now().mesh, kind, bytes, signature)
            .map_err(|error| Error::Access(Box::new(error)))?;
        Ok(Signed {
            hello: admitted.hello().clone(),
            signature: *kept,
        })
    }

    /// Waits until the admitted hello ends, and gives the refusal. A renewal moves the
    /// end of the wait at its next poll, so poll it in the task that admits each
    /// renewal.
    ///
    /// # Panics
    ///
    /// When no hello is admitted, or the mesh time is lost after the hello.
    pub(crate) async fn expiry(&self, state: &RefCell<State>) -> Error {
        loop {
            let (ends, reach) = {
                let state = state.borrow();
                let ends = self.ends();
                let now = state
                    .time
                    .now()
                    .mesh
                    .expect("invariant: mesh time stays once the clock has synced");
                if now.latest >= ends {
                    return Error::Access(Box::new(proof::Error::Expired {
                        expires: ends,
                        now: now.latest,
                    }));
                }
                (ends, state.time.reach(ends))
            };
            let mut reach = pin!(reach);
            poll_fn(|cx| {
                if self.ends() != ends {
                    return Poll::Ready(());
                }
                reach.as_mut().poll(cx)
            })
            .await;
        }
    }

    /// The end of the admitted hello.
    fn ends(&self) -> Stamp {
        self.admitted
            .borrow()
            .as_ref()
            .expect("invariant: a session waits only after its hello")
            .0
            .ends()
    }
}

/// A wait for the renewals of a session. It keeps at most one waker, and its drop takes
/// it out.
pub(crate) struct Renewal<'a> {
    admission: &'a Admission,
    key: u64,
    /// The count of hellos admitted through the last hello that it gave.
    seen: Cell<u64>,
    /// The waker that the set holds for it, and the count of hellos admitted when the
    /// waker went in.
    held: Cell<Option<(Waker, u64)>>,
}

impl Renewal<'_> {
    /// The admitted hello and its signature. A later [`Renewal::renewed`] gives only a
    /// hello admitted after it.
    ///
    /// # Panics
    ///
    /// When no hello is admitted.
    pub(crate) fn signed(&self) -> Signed {
        let (signed, seen) = self.admission.signed();
        self.seen.set(seen);
        signed
    }

    /// The admitted hello when the session admitted it after the last hello that this
    /// renewal gave, else `None`.
    pub(crate) fn renewed(&self) -> Option<Signed> {
        (self.admission.count.get() > self.seen.get()).then(|| self.signed())
    }

    /// Keeps the waker of `cx` until the session admits the next hello.
    pub(crate) fn register(&self, cx: &Context<'_>) {
        let count = self.admission.count.get();
        let held = self.held.take();
        if let Some((waker, at)) = &held
            && *at == count
            && waker.will_wake(cx.waker())
        {
            self.held.set(held);
            return;
        }
        let replaced = self
            .admission
            .waiting
            .borrow_mut()
            .insert(self.key, cx.waker());
        self.held.set(Some((cx.waker().clone(), count)));
        // The last drop of a waker can drop a task that borrows the map.
        drop(replaced);
        drop(held);
    }
}

impl Drop for Renewal<'_> {
    fn drop(&mut self) {
        let held = self.admission.waiting.borrow_mut().remove(self.key);
        // As in `register`.
        drop(held);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use types::ed25519::PrivateKey;

    use super::*;

    /// A task that counts its wakes.
    #[derive(Default)]
    struct Task(AtomicUsize);

    impl Wake for Task {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Task {
        fn wakes(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn task() -> (Arc<Task>, Waker) {
        let task = Arc::new(Task::default());
        let waker = Waker::from(Arc::clone(&task));
        (task, waker)
    }

    fn admission() -> Admission {
        Admission::new(PrivateKey([3; 32]).public())
    }

    /// Takes the entry out by hand, which no production path does without a count
    /// or a drop, to show that the second poll does not touch the set.
    #[test]
    fn a_poll_with_the_same_waker_leaves_the_set_alone() {
        let admission = admission();
        let renewal = admission.renewal();
        let (task, waker) = task();
        renewal.register(&Context::from_waker(&waker));
        let taken = admission.waiting.borrow_mut().remove(renewal.key);
        assert!(taken.is_some_and(|taken| taken.will_wake(&waker)));
        renewal.register(&Context::from_waker(&waker.clone()));
        assert!(admission.waiting.borrow_mut().remove(renewal.key).is_none());
        admission.wake();
        assert_eq!(task.wakes(), 0);
    }

    #[test]
    fn a_poll_with_another_task_inserts_its_waker() {
        let admission = admission();
        let renewal = admission.renewal();
        let (first, waker) = task();
        let (second, other) = task();
        renewal.register(&Context::from_waker(&waker));
        renewal.register(&Context::from_waker(&other));
        admission.wake();
        assert_eq!([first.wakes(), second.wakes()], [0, 1]);
    }

    /// The drain of an admit empties the set, so the next poll with the same waker
    /// must insert it again, or the next admit loses its wake.
    #[test]
    fn a_poll_after_an_admit_registers_again() {
        let admission = admission();
        let renewal = admission.renewal();
        let (task, waker) = task();
        renewal.register(&Context::from_waker(&waker));
        admission.wake();
        assert_eq!(task.wakes(), 1);
        renewal.register(&Context::from_waker(&waker));
        admission.wake();
        assert_eq!(task.wakes(), 2);
    }

    #[test]
    fn a_drop_takes_its_waker_out() {
        let admission = admission();
        let (task, waker) = task();
        let renewal = admission.renewal();
        renewal.register(&Context::from_waker(&waker));
        drop(renewal);
        admission.wake();
        assert_eq!(task.wakes(), 0);
    }
}

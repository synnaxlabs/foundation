//! The hello that a session admitted, its renewals, and its end.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::pin::pin;
use std::task::{Poll, Waker};

use access::proof::{self, Admitted};
use types::ed25519::PublicKey;
use types::name::Name;
use types::time::Stamp;
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
    /// The waker of each task that waits for a hello after a count.
    waiting: RefCell<Vec<Waker>>,
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
        let state = state.borrow();
        let now = state.time.now().mesh;
        let mut admitted = self.admitted.borrow_mut();
        let next = match admitted.as_ref() {
            None => state.rules.admit(now, self.peer, hello, &signature),
            Some((first, _)) => state.rules.renew(first, now, hello, &signature),
        }
        .map_err(|error| Error::Access(Box::new(error)))?;
        *admitted = Some((next, signature));
        self.count.set(self.count.get() + 1);
        for waker in self.waiting.take() {
            waker.wake();
        }
        Ok(())
    }

    /// The admitted hello and its signature, with the count of hellos admitted
    /// through it.
    ///
    /// # Panics
    ///
    /// When no hello is admitted.
    pub(crate) fn signed(&self) -> (Signed, u64) {
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

    /// Waits until the session admits a hello after the first `count`.
    pub(crate) async fn renewed(&self, count: u64) {
        poll_fn(|cx| {
            if self.count.get() > count {
                return Poll::Ready(());
            }
            let mut waiting = self.waiting.borrow_mut();
            if !waiting.iter().any(|waker| waker.will_wake(cx.waker())) {
                waiting.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await;
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

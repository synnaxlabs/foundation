//! The hello that a session admitted, its renewals, and its end.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;

use access::proof::{self, Admitted};
use types::name::Name;
use types::node;
use types::time::Stamp;
use wire::hub::client::Signed;

use super::Error;
use crate::State;
use crate::task::Waiter;

/// The hello that a session admitted last, and its signature. The first
/// [`Admission::admit`] admits a hello, and each later one renews it.
#[derive(Debug)]
pub(crate) struct Admission {
    /// The node that carried each hello of the session.
    peer: node::Key,
    admitted: RefCell<Option<(Admitted, [u8; 64])>>,
    /// The wait of [`Admission::expiry`], woken at each renewal.
    expiry: RefCell<Waiter>,
}

impl Admission {
    /// An admission of the hellos that `peer` carries: this node for a client
    /// session.
    pub(crate) fn new(peer: node::Key) -> Self {
        Self {
            peer,
            admitted: RefCell::default(),
            expiry: RefCell::default(),
        }
    }

    /// Admits `signed`, or renews the hello that the session holds with it. Checks no
    /// nonce: the caller checks it.
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
        .map_err(Error::Access)?;
        *admitted = Some((next, signature));
        self.expiry.borrow().wake();
        Ok(())
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
        let (admitted, hello) = admitted
            .as_ref()
            .expect("invariant: a session verifies only after its hello");
        state
            .rules
            .verify(admitted, state.time.now().mesh, kind, bytes, signature)
            .map_err(Error::Access)?;
        Ok(Signed {
            hello: admitted.hello().clone(),
            signature: *hello,
        })
    }

    /// Waits until the admitted hello ends, and gives the refusal. Each renewal moves
    /// the end of the wait.
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
                    return Error::Access(proof::Error::Expired {
                        expires: ends,
                        now: now.latest,
                    });
                }
                (ends, state.time.reach(ends))
            };
            let mut reach = pin!(reach);
            poll_fn(|cx| {
                if self.ends() != ends {
                    return Poll::Ready(());
                }
                self.expiry.borrow_mut().polled(cx);
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

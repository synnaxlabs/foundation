//! The hello that a session admitted, its renewals, and its end.

use std::cell::RefCell;

use access::proof::{self, Admitted};
use types::name::Name;
use types::node;
use wire::hub::client::Signed;

use super::Error;
use crate::State;

/// The hello that a session admitted last, and its signature. The first
/// [`Admission::take`] admits a hello, and each later one renews it.
#[derive(Debug, Default)]
pub(crate) struct Admission(RefCell<Option<(Admitted, [u8; 64])>>);

impl Admission {
    /// Admits `signed`, whose hello `peer` carried, or renews the hello that the
    /// session holds with it. Checks no nonce: the caller checks it.
    ///
    /// # Errors
    ///
    /// [`Error::Access`] when the rules refuse the hello.
    pub(crate) fn take(
        &self,
        state: &RefCell<State>,
        peer: node::Key,
        signed: Signed,
    ) -> Result<(), Error> {
        let Signed { hello, signature } = signed;
        let state = state.borrow();
        let now = state.time.now().mesh;
        let mut admitted = self.0.borrow_mut();
        let next = match admitted.as_ref() {
            None => state.rules.admit(now, peer, hello, &signature),
            Some((first, _)) => state.rules.renew(first, now, hello, &signature),
        }
        .map_err(Error::Access)?;
        *admitted = Some((next, signature));
        Ok(())
    }

    /// The subject of the admitted hello, or `None` before one. A renewal never
    /// changes the subject.
    pub(crate) fn subject(&self) -> Option<Name> {
        self.0
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
        let admitted = self.0.borrow();
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

    /// Waits until the admitted hello ends, and gives the refusal. A renewal can move
    /// the end earlier, so the caller starts a new wait after each renewal.
    ///
    /// # Panics
    ///
    /// When no hello is admitted, or the mesh time is lost after the hello.
    pub(crate) async fn expiry(&self, state: &RefCell<State>) -> Error {
        loop {
            let wait = {
                let state = state.borrow();
                let admitted = self.0.borrow();
                let (admitted, _) = admitted
                    .as_ref()
                    .expect("invariant: a session waits only after its hello");
                let ends = admitted.ends();
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
                state.time.reach(ends)
            };
            wait.await;
        }
    }
}

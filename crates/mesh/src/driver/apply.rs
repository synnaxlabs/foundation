//! Applies a spec through the leader of the group.

use std::collections::BTreeMap;

use spec::definition::Definition;
use spec::tree::Chunks;
use types::name::Name;

use super::{Mesh, TICK};
use crate::change::{CHUNKS_MAX, Change};
use crate::error::Error;
use crate::pointer::Pointer;
use crate::region::Refused;

impl Mesh {
    /// Makes `definitions`, by tree key, the region's spec, when the pointer is still
    /// `base`. A follower forwards the change to the leader. Returns the new pointer
    /// once its entry has committed and this node applied it. It tries again when a
    /// new leader replaces the entry, and after each tick while no leader takes it, as
    /// [`Mesh::set_home`] does.
    ///
    /// # Errors
    ///
    /// Each error but `Stale` proposes nothing, and `Problems` and `Large` come before
    /// the others.
    ///
    /// - [`Error::Problems`] when the spec has problems.
    /// - [`Error::Large`] when its tree has more chunks than one change lists.
    /// - [`Error::Stale`] when the pointer is not `base` at the apply.
    /// - [`Error::NoVote`] and [`Error::Stopped`] as for [`Mesh::set_home`].
    ///
    /// # Panics
    ///
    /// On a broken invariant of the region state: a refusal of the change that is not
    /// `Stale`, or a version past `u64::MAX`.
    pub async fn apply(
        &self,
        base: Pointer,
        definitions: BTreeMap<Name, Definition>,
    ) -> Result<Pointer, Error> {
        let problems =
            spec::region::check(self.group.borrow().state.region(), &definitions);
        if !problems.is_empty() {
            return Err(Error::Problems(problems));
        }
        let update = spec::region::tree(&mut Chunks::default(), &definitions);
        if update.chunks.len() > CHUNKS_MAX {
            return Err(Error::Large {
                chunks: update.chunks.len(),
                most: CHUNKS_MAX,
            });
        }
        let root = update.root;
        let change = Change::Spec {
            base,
            root,
            chunks: update.chunks.into_iter().collect(),
        };
        loop {
            let attempt = self.attempt()?;
            let Some(at) = attempt.place(change.clone()).await? else {
                drop(attempt);
                self.clock.sleep(TICK).await;
                continue;
            };
            match attempt.applied(at).await? {
                Some(Ok(())) => {
                    let version = base
                        .version
                        .checked_add(1)
                        .expect("invariant: fewer than 2^64 spec changes apply");
                    return Ok(Pointer { version, root });
                }
                Some(Err(Refused::Stale { base, pointer })) => {
                    return Err(Error::Stale { base, pointer });
                }
                Some(Err(refused)) => {
                    panic!(
                        "invariant: a spec change is refused only as stale: {refused}"
                    )
                }
                None => {}
            }
        }
    }
}

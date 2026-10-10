//! Applies a spec through the leader of the group.

use std::collections::{BTreeMap, BTreeSet};

use spec::Pointer;
use spec::channel::{Channel, Kind};
use spec::definition::Definition;
use spec::tree::{self, Chunks, Update};
use types::channel;
use types::digest::Digest;
use types::name::Name;
use types::node;

use super::{Mesh, put};
use crate::change::{self, CHUNKS_MAX, Change, HOMES_MAX};
use crate::error::Error;
use crate::region::{self, Refused};

impl Mesh {
    /// Makes `definitions`, by tree key, the region's spec, when the pointer is still
    /// `base`. `homes` gives the home node of an index, by index name to node name. The
    /// change gives its listed home to each index of `homes` that has no home in this
    /// node's state, and at the apply, an index that has a home keeps it. On `Ok`, a
    /// put of each chunk of the new tree in [`Config::store`](super::Config::store) has
    /// returned. The change lists each chunk of the new tree that the tree of `base`
    /// lacks, or each chunk of the new tree when the store cannot give the tree of
    /// `base`. A follower forwards the change to the leader. Returns the pointer once
    /// its entry has committed and this node applied it: `base` when the tree of
    /// `definitions` is the tree of `base` and each index of `homes` has a home in this
    /// node's state, as such a change leaves the pointer, and else a new pointer. It
    /// tries again when a new leader replaces the entry, and after each tick while no
    /// leader takes it, as [`Mesh::set_home`] does. On `Ok`, the pointer is the one
    /// this call makes, and each index of `homes` has a home in this node's state when
    /// the call settles, the listed one or another. A call whose entry finds that
    /// pointer, after a lost answer or an equal change of another call, returns it when
    /// each index of `homes` has a home then, and else gives `Stale`. A retry that
    /// finds a later pointer gives `Stale`, even when an entry of this call applied
    /// before it.
    ///
    /// # Errors
    ///
    /// `Problems` comes first, then `NotIndex`, `Homes`, a `Blob` from the read of the
    /// tree of `base`, `Large`, `Stopped`, `NoVote`, `UnknownNode`, and a `Quorum`
    /// before the first put. None of these, `Pool`, and `Blob` propose anything.
    ///
    /// - [`Error::Problems`] when the spec has problems.
    /// - [`Error::NotIndex`] when `definitions` does not hold an index of `homes` as
    ///   an index channel.
    /// - [`Error::Homes`] when more indexes of `homes` have no home than one change can
    ///   give.
    /// - [`Error::Large`] when the change lists more chunks than one change can list.
    /// - [`Error::NoVote`] and [`Error::Stopped`] as for [`Mesh::set_home`].
    /// - [`Error::UnknownNode`] when no member of the region has the name of the node
    ///   of an index of `homes` that has no home. It reads what this node applied.
    /// - [`Error::Pool`] when the pool has no block for a chunk, and [`Error::Blob`]
    ///   when a call of the store fails.
    /// - [`Error::Quorum`] when the voters that hold the chunks are not a majority of
    ///   each half of the voters, before the proposal or at the apply.
    /// - [`Error::Stale`] when the pointer at the apply of its entry is not `base`, and
    ///   either it is not the pointer this call makes, or an index of `homes` has no
    ///   home in this node's state when the call settles.
    ///
    /// # Panics
    ///
    /// On a broken invariant of the region state: a refusal of the change that is not
    /// `Stale` or `Quorum`, or a version past `u64::MAX`.
    pub async fn apply(
        &self,
        base: Pointer,
        definitions: BTreeMap<Name, Definition>,
        homes: BTreeMap<Name, Name>,
    ) -> Result<Pointer, Error> {
        let problems =
            spec::region::check(self.group.borrow().state.prefix(), &definitions);
        if !problems.is_empty() {
            return Err(Error::Problems(problems));
        }
        let mut indexes = Vec::with_capacity(homes.len());
        for (index, home) in homes {
            match definitions.get(&index) {
                Some(Definition::Channel(Channel {
                    key,
                    kind: Kind::Index { .. },
                })) => indexes.push((*key, home)),
                _ => return Err(Error::NotIndex(index)),
            }
        }
        // No entry removes a home, so `keyed` keeps at most this many.
        let unhomed = unhomed(&self.group.borrow().state, &indexes).count();
        if unhomed > HOMES_MAX {
            return Err(Error::Homes {
                homes: unhomed,
                most: HOMES_MAX,
            });
        }
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &definitions);
        let root = update.root;
        let listed = self.listed(&mut chunks, base.root, &update).await?;
        if listed.len() > CHUNKS_MAX {
            return Err(Error::Large {
                chunks: listed.len(),
                most: CHUNKS_MAX,
            });
        }
        // A try opens only after the puts, since its floor keeps `Applied` from a trim.
        self.check_proposer()?;
        let homes = self.keyed(&indexes)?;
        let holders = BTreeSet::from([self.group.borrow().raft.key()]);
        region::quorum(self.group.borrow().raft.voters(), &holders).map_err(refused)?;
        // Each chunk, not only the listed ones: the store can lack a chunk that the
        // base shares with the new tree, and `diff` never reads a shared chunk.
        put(&self.store, &self.pool, &chunks, &update.chunks).await?;
        let listed = listed.into_iter().collect();
        self.settle_spec(base, root, listed, holders, homes).await
    }

    // The home of each index of `indexes` that has no home, by the key of its node,
    // as this node applied.
    fn keyed(
        &self,
        indexes: &[(channel::Key, Name)],
    ) -> Result<BTreeMap<channel::Key, node::Key>, Error> {
        let group = self.group.borrow();
        let keyed = unhomed(&group.state, indexes).map(|(index, home)| {
            let key = group
                .state
                .named(home)
                .ok_or_else(|| Error::UnknownNode(home.clone()))?;
            Ok((*index, key))
        });
        keyed.collect()
    }

    // Proposes the `Spec` change of `base`, `root`, `chunks`, `holders`, and `homes`,
    // one try at a time, until it applies, and gives the pointer it makes.
    pub(super) async fn settle_spec(
        &self,
        base: Pointer,
        root: Digest,
        chunks: BTreeSet<Digest>,
        holders: BTreeSet<node::Key>,
        homes: BTreeMap<channel::Key, node::Key>,
    ) -> Result<Pointer, Error> {
        let change = Change::Spec {
            base,
            root,
            chunks,
            holders,
            homes: homes.clone(),
        };
        loop {
            match self.attempt()?.settle(change.clone()).await? {
                Some(Ok(())) => return Ok(change::pointer(base, root, &homes)),
                // `change::pointer` panics on a base at the last version.
                Some(Err(Refused::Stale { pointer, .. }))
                    if base.version < u64::MAX
                        && pointer == change::pointer(base, root, &homes)
                        && self.homed(&homes) =>
                {
                    return Ok(pointer);
                }
                Some(Err(rest)) => return Err(refused(rest)),
                None => {}
            }
        }
    }

    // Whether each index of `homes` has a home, as this node applied.
    fn homed(&self, homes: &BTreeMap<channel::Key, node::Key>) -> bool {
        let group = self.group.borrow();
        homes.keys().all(|&index| group.state.home(index).is_some())
    }

    // The chunks of the tree of `update` that the tree at `base` lacks, or each chunk
    // of `update` when the store lacks a chunk of the tree at `base` or holds one that
    // does not fit in a tree. `chunks` holds the tree of `update`, and takes each
    // chunk that the store gives.
    async fn listed(
        &self,
        chunks: &mut Chunks,
        base: Digest,
        update: &Update,
    ) -> Result<Vec<Digest>, Error> {
        loop {
            let missing = match tree::diff(chunks, base, update.root) {
                Ok(diff) => return Ok(diff.chunks),
                Err(tree::Error::Missing(digest)) => digest,
                Err(tree::Error::Corrupt(_)) => return Ok(update.chunks.clone()),
            };
            match self.store.get(missing).await.map_err(Error::Blob)? {
                Some(chunk) => chunks.insert(chunk.to_vec()),
                None => return Ok(update.chunks.clone()),
            };
        }
    }
}

// Each index of `indexes` that has no home in `state`.
fn unhomed<'a>(
    state: &'a region::State,
    indexes: &'a [(channel::Key, Name)],
) -> impl Iterator<Item = &'a (channel::Key, Name)> {
    indexes
        .iter()
        .filter(|&&(index, _)| state.home(index).is_none())
}

// The error of a refused spec change.
fn refused(cause: Refused) -> Error {
    match cause {
        Refused::Stale { base, pointer } => Error::Stale { base, pointer },
        Refused::Quorum { held, voters } => Error::Quorum { held, voters },
        cause => panic!(
            "invariant: a spec change is refused only as stale or for its quorum: \
             {cause}"
        ),
    }
}

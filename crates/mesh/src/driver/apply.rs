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
use crate::change::{CHUNKS_MAX, Change, HOMES_MAX};
use crate::error::Error;
use crate::region::{self, Refused};

impl Mesh {
    /// Makes `definitions`, by tree key, the region's spec, when the pointer is still
    /// `base`. `homes` gives the home node of each index that has no home, by index
    /// name to node name. At the apply, each index that has no home gets its listed
    /// one, and a listed index that has a home keeps it. On `Ok`, a put of each chunk of the new tree in
    /// [`Config::store`](super::Config::store) has returned. The change lists each
    /// chunk of the new tree that the tree of `base` lacks, or each chunk of the new
    /// tree when the store cannot give the tree of `base`. A follower forwards the
    /// change to the leader. Returns the new pointer once its entry has committed and
    /// this node applied it. It tries again when a new leader replaces the entry, and
    /// after each tick while no leader takes it, as [`Mesh::set_home`] does. A call
    /// whose entry finds the pointer that the call makes, after a lost answer or an
    /// equal change of another call, returns that pointer. A retry that finds a later
    /// pointer gives `Stale`, even when an entry of this call applied before it.
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
    /// - [`Error::Homes`] when `homes` holds more homes than one change can give.
    /// - [`Error::Large`] when the change lists more chunks than one change can list.
    /// - [`Error::NoVote`] and [`Error::Stopped`] as for [`Mesh::set_home`].
    /// - [`Error::UnknownNode`] when no member of the region has the name of a node
    ///   of `homes`. It reads what this node applied.
    /// - [`Error::Pool`] when the pool has no block for a chunk, and [`Error::Blob`]
    ///   when a call of the store fails.
    /// - [`Error::Quorum`] when the voters that hold the chunks are not a majority of
    ///   each half of the voters, before the proposal or at the apply.
    /// - [`Error::Stale`] when the pointer is not `base`, or the pointer this call
    ///   makes, at the apply.
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
        if indexes.len() > HOMES_MAX {
            return Err(Error::Homes {
                homes: indexes.len(),
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
        let homes = self.keyed(indexes)?;
        let holders = BTreeSet::from([self.key()]);
        region::quorum(self.group.borrow().raft.voters(), &holders).map_err(refused)?;
        // Each chunk, not only the listed ones: the store can lack a chunk that the
        // base shares with the new tree, and `diff` never reads a shared chunk.
        put(&self.store, &self.pool, &chunks, &update.chunks).await?;
        let listed = listed.into_iter().collect();
        self.settle_spec(base, root, listed, holders, homes).await
    }

    // The home of each index of `indexes` by the key of its node, as this node
    // applied the members.
    fn keyed(
        &self,
        indexes: Vec<(channel::Key, Name)>,
    ) -> Result<BTreeMap<channel::Key, node::Key>, Error> {
        let group = self.group.borrow();
        let keyed = indexes.into_iter().map(|(index, home)| {
            let key = group.state.named(&home).ok_or(Error::UnknownNode(home))?;
            Ok((index, key))
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
            homes,
        };
        loop {
            match self.attempt()?.settle(change.clone()).await? {
                Some(Ok(())) => return Ok(base.next(root)),
                Some(Err(Refused::Stale { pointer, .. }))
                    if pointer.root == root
                        && pointer.version.checked_sub(1) == Some(base.version) =>
                {
                    return Ok(pointer);
                }
                Some(Err(rest)) => return Err(refused(rest)),
                None => {}
            }
        }
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

//! The one path for every read and write: writer and reader sessions on the channels
//! of a shard's home.

mod channel;
pub mod client;
mod commit;
mod link;
pub mod reader;
pub mod serve;
pub mod writer;

use std::cell::{Cell, RefCell};
use std::hash::Hash;
use std::rc::Rc;
use std::task::Waker;

use spec::channel::Kind;
use spec::definition::Definition;
use types::channel::Key;
use types::frame::key_set::Interner;
use types::hash;
use types::name::Name;

use channel::Channel;
pub use link::{Link, Served};
use reader::Reader;
use writer::Writer;

/// The `home` items that hub calls give, so that layer 3 names them through `hub`.
pub mod home {
    pub use ::home::{Error, Outcome, Refusal};

    /// Why the home refused the stamps of a frame.
    pub mod order {
        pub use ::home::order::Error;
    }

    /// Why the home refused a writer.
    pub mod writer {
        pub use ::home::writer::Error;
    }
}

/// The `mesh` items that hub calls give, so that layer 3 names them through `hub`.
pub mod mesh {
    pub use ::mesh::Stopped;
}

/// The hub of one shard: it opens writer and reader sessions on the indexes of the
/// shard's home, and serves each hub stream of a transport session that its caller
/// gives to a [`Link`]. It is not `Send`: each call is on the shard's thread. Clones
/// share it.
#[derive(Clone, Debug)]
pub struct Hub(Rc<RefCell<State>>);

/// What one shard's hub is given at start.
#[derive(Debug)]
pub struct Config {
    /// The shard's home. Only the hub calls it.
    pub home: ::home::Shard,
    /// The node's key set interner, which owns the slot table that the home's buffer
    /// opened with.
    pub interner: Interner,
    /// Where the hub spawns its commit task.
    pub tasks: env::tasks::Tasks,
    /// This node's key. A client's hello must name it as `via`.
    pub node: types::node::Key,
    /// Mesh time, which the hub checks each hello and request against.
    pub time: clock::Reader,
    /// The source of each challenge's nonce.
    pub entropy: env::entropy::Entropy,
    /// The region's mesh, or `None` for a node with no region. The mesh names the home
    /// of each index and the address of each member. With `None`, this node is the
    /// home of each index.
    pub mesh: Option<::mesh::Mesh>,
}

/// The state of one shard's hub, which each session shares. No borrow of it lasts
/// past a call or across an `.await`.
#[derive(Debug)]
struct State {
    home: ::home::Shard,
    interner: Interner,
    channels: hash::Map<Name, Channel>,
    /// The index of each channel in `channels`, by key.
    indexes: hash::Map<Key, Key>,
    /// Each open writer, by its home key.
    writers: Sessions<::home::writer::Key>,
    readers: Sessions<::home::reader::Key>,
    /// The waker of each reader that waits for a frame.
    wakers: hash::Map<::home::reader::Key, Waker>,
    /// The readers that [`::home::Shard::woken`] gave last.
    woken: Vec<::home::reader::Key>,
    commit: commit::Signal,
    /// The error that ended the home's buffer.
    failed: Option<env::files::Error>,
    node: types::node::Key,
    time: clock::Reader,
    entropy: env::entropy::Entropy,
    /// Empty, so refusing each hello, until [`Hub::set_rules`] first runs.
    rules: access::Rules,
    mesh: Option<::mesh::Mesh>,
}

impl Hub {
    /// A hub over `config.home` that knows no channel yet. Spawns a task on
    /// `config.tasks` that ends when the home's buffer fails, or once the hub and each
    /// of its sessions have dropped. Once the hub and each of its sessions drop, it
    /// holds no part of the home. So a `::home::Commit` taken before `new` and awaited
    /// after that drop resolves once the buffer's task ended, and when the caller holds
    /// no other part of the home, the ring closes when that commit drops.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let Config {
            home,
            interner,
            tasks,
            node,
            time,
            entropy,
            mesh,
        } = config;
        let state = Rc::new(RefCell::new(State {
            home,
            interner,
            channels: hash::Map::default(),
            indexes: hash::Map::default(),
            writers: Sessions::default(),
            readers: Sessions::default(),
            wakers: hash::Map::default(),
            woken: Vec::new(),
            commit: commit::Signal::default(),
            failed: None,
            node,
            time,
            entropy,
            rules: access::Rules::default(),
            mesh,
        }));
        tasks.spawn(commit::run(Rc::downgrade(&state)));
        Self(state)
    }

    /// Makes the channels of `definitions`, in any order, the channels that sessions
    /// may name. It skips each definition that is not a channel. A known channel whose
    /// key, name, and definition stay keeps its sessions. Each other known channel is
    /// removed, and each session on it ends at once: [`writer::Failure::Removed`],
    /// [`reader::Ended::Removed`], and [`serve::Error::Removed`]. Then each new
    /// channel is defined. The home stops carrying each index whose key is not an index
    /// of `definitions`, and carries an index from the first session that finds this
    /// node is its home. A reader of a new channel at the key of a removed data channel
    /// takes no series of the removed one.
    ///
    /// # Panics
    ///
    /// Before any change, when two channels of `definitions` have one key or one
    /// name, or the index of a data channel is not an index of `definitions`.
    pub fn set_definitions<'d>(
        &self,
        definitions: impl IntoIterator<Item = (&'d Name, &'d Definition)>,
    ) {
        let channels: Vec<_> = definitions
            .into_iter()
            .filter_map(|(name, definition)| match definition {
                Definition::Channel(channel) => Some((name, channel)),
                _ => None,
            })
            .collect();
        self.0.borrow_mut().set(&checked(&channels));
    }

    /// Opens a writer session on `config.channels` and the index of each. While the
    /// mesh names no home for an index, it waits for one.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Empty`] for no name, [`writer::Error::Unknown`] for the first
    /// name that no channel has, then, for the first index whose home is not this
    /// node, [`writer::Error::Remote`], or [`writer::Error::Mesh`] when the mesh
    /// stopped. Else [`writer::Error::Home`] when the home refuses the writer.
    pub async fn writer(
        &self,
        config: writer::Config,
    ) -> Result<Writer, writer::Error> {
        Writer::open(&self.0, config).await
    }

    /// Opens a reader session on `channels`, which share one index, as
    /// [`writer`](Self::writer) opens a writer. It gets each frame of the index, as a
    /// view of only `channels` and their index. A complete reader gets each live frame
    /// written after the returned future resolves, until it misses one
    /// ([`reader::Mode::Complete`]).
    ///
    /// # Errors
    ///
    /// For the first name that breaks a rule: [`reader::Error::Unknown`] for a name
    /// that no channel has, and [`reader::Error::ManyIndexes`] for a channel on
    /// another index than the first. [`reader::Error::Empty`] for no name. Then
    /// [`reader::Error::Remote`] when the home of the index is not this node, and
    /// [`reader::Error::Mesh`] when the mesh stopped.
    pub async fn reader(
        &self,
        channels: &[Name],
        mode: reader::Mode,
    ) -> Result<Reader, reader::Error> {
        Reader::open(&self.0, channels, mode).await
    }

    /// Sets the access rules that each later hello and request is checked against.
    /// Until the first call, the rules know no subject, so they refuse each hello with
    /// `access::proof::Error::Unknown`.
    pub fn set_rules(&self, rules: access::Rules) {
        self.0.borrow_mut().rules = rules;
    }

    /// The hub's part of `session`. Give to [`Link::serve`] each hub stream of the
    /// session that the caller does not reject.
    #[must_use]
    pub fn link(&self, session: transport::Session) -> Link {
        Link::new(Rc::clone(&self.0), session)
    }
}

/// The channels of `channels`, by name.
///
/// # Panics
///
/// As [`Hub::set_definitions`].
fn checked<'d>(
    channels: &[(&'d Name, &'d spec::channel::Channel)],
) -> hash::Map<&'d Name, &'d spec::channel::Channel> {
    let mut named = hash::Map::default();
    let mut kinds = hash::Map::default();
    for &(name, channel) in channels {
        let key = channel.key;
        assert!(
            named.insert(name, channel).is_none(),
            "two channels are named {name}"
        );
        assert!(
            kinds.insert(key, &channel.kind).is_none(),
            "two channels have key {key}"
        );
    }
    for &(name, channel) in channels {
        if let Kind::Data(data) = &channel.kind {
            let index = *data.index();
            assert!(
                matches!(kinds.get(&index), Some(Kind::Index { .. })),
                "the index {index} of channel {name} is not an index of the definitions"
            );
        }
    }
    named
}

/// The open sessions of one kind, by home key, with the channels of each. A session
/// is open while it is here.
#[derive(Debug)]
struct Sessions<K>(hash::Map<K, Open>);

/// The channels of an open session, and its removal.
#[derive(Debug)]
struct Open {
    keys: Box<[Key]>,
    removal: Removal,
}

/// The first channel of a session that the hub removed, which the session reads at
/// each call. It reads no map, so its check costs the same while other sessions end.
#[derive(Clone, Debug, Default)]
pub(crate) struct Removal(Rc<Cell<Option<Key>>>);

impl Removal {
    /// The key of the removed channel, or `None` while each channel stays.
    pub(crate) fn get(&self) -> Option<Key> {
        self.0.get()
    }
}

impl<K> Default for Sessions<K> {
    fn default() -> Self {
        Self(hash::Map::default())
    }
}

impl<K: Copy + Ord + Hash> Sessions<K> {
    /// Opens the session `key` on `keys`, so that a removal of one of them ends it.
    fn add(&mut self, key: K, keys: Box<[Key]>) -> Removal {
        let removal = Removal::default();
        let open = Open {
            keys,
            removal: removal.clone(),
        };
        let added = self.0.insert(key, open);
        assert!(added.is_none(), "invariant: the home gives each key once");
        removal
    }

    /// Returns whether the session `key` was open, and makes it not open.
    fn remove(&mut self, key: K) -> bool {
        self.0.remove(&key).is_some()
    }

    /// Puts the first channel of `removed` in the removal of each session on one of
    /// them. Returns their keys in order, to close.
    fn end(&self, removed: &hash::Set<Key>) -> Vec<K> {
        let mut ended: Vec<K> = self
            .0
            .iter()
            .filter_map(|(&key, open)| {
                let first = open.keys.iter().find(|key| removed.contains(key))?;
                open.removal.0.set(Some(*first));
                Some(key)
            })
            .collect();
        ended.sort_unstable();
        ended
    }
}

impl State {
    /// Makes `channels` the known channels, as [`Hub::set_definitions`] says.
    fn set(&mut self, channels: &hash::Map<&Name, &spec::channel::Channel>) {
        let removed: hash::Set<Key> = self
            .channels
            .iter()
            .filter(|&(name, known)| channels.get(name) != Some(&&known.0))
            .map(|(_, known)| known.key())
            .collect();
        let index = |channel: &spec::channel::Channel| {
            matches!(channel.kind, Kind::Index { .. }).then_some(channel.key)
        };
        let before: hash::Set<Key> = self
            .channels
            .values()
            .filter_map(|known| index(&known.0))
            .collect();
        let after: hash::Set<Key> = channels
            .values()
            .filter_map(|channel| index(channel))
            .collect();
        self.end(&removed);
        let mut shed: Vec<Key> = before.difference(&after).copied().collect();
        shed.sort_unstable();
        for key in shed {
            let slot = self.interner.slots().index(key);
            self.home.shed(slot);
        }
        let slots = self.interner.slots();
        self.channels.retain(|_, known| {
            let gone = removed.contains(&known.key());
            if gone {
                slots.retire(known.key());
            }
            !gone
        });
        self.indexes.retain(|key, _| !removed.contains(key));
        let mut new: Vec<_> = channels
            .iter()
            .filter(|&(name, _)| !self.channels.contains_key(*name))
            .collect();
        new.sort_unstable_by_key(|&(name, _)| *name);
        for (name, channel) in new {
            self.define(name, channel);
        }
    }

    /// Ends each session on a channel of `removed`, and closes it at the home.
    fn end(&mut self, removed: &hash::Set<Key>) {
        for key in self.writers.end(removed) {
            self.close_writer(key);
        }
        for key in self.readers.end(removed) {
            if let Some(waker) = self.close_reader(key) {
                waker.wake();
            }
        }
    }

    /// Closes the writer `key` at the home, unless a removal closed it.
    fn close_writer(&mut self, key: ::home::writer::Key) {
        if self.writers.remove(key) {
            self.home.close_writer(key);
            self.commit.appended();
        }
    }

    /// Closes the reader session `key` at the home, unless a removal closed it.
    /// Returns its waker, when it waits for a frame.
    fn close_reader(&mut self, key: ::home::reader::Key) -> Option<Waker> {
        let waker = self.wakers.remove(&key);
        if self.readers.remove(key) {
            self.home.close_reader(key);
        }
        waker
    }

    /// Makes `channel` known to sessions as `name`.
    fn define(&mut self, name: &Name, channel: &spec::channel::Channel) {
        let channel = Channel(channel.clone());
        self.indexes.insert(channel.key(), channel.index());
        self.channels.insert(name.clone(), channel);
    }

    /// Carries `index` at the home. A later carry does nothing.
    fn carry(&mut self, index: types::channel::Key) {
        let slot = self.interner.slots().index(index);
        self.home.carry(slot);
    }

    /// Carries `index` at the home, and gives the slot of each of `keys` in its role:
    /// `index` as an index, and each other key as a data channel.
    fn slots(&mut self, index: Key, keys: &[Key]) -> Box<[types::channel::Slot]> {
        self.carry(index);
        let assigned = self.interner.slots();
        let slot = assigned.index(index);
        keys.iter()
            .map(|&key| {
                if key == index {
                    slot
                } else {
                    assigned.data(key)
                }
            })
            .collect()
    }

    /// Wakes each reader that the home names as having a frame to take or a miss to
    /// report.
    fn wake(&mut self) {
        self.home.woken(&mut self.woken);
        for key in &self.woken {
            if let Some(waker) = self.wakers.remove(key) {
                waker.wake();
            }
        }
    }

    /// Keeps `error`, which ended the home's buffer, and wakes every reader.
    fn fail(&mut self, error: env::files::Error) {
        self.failed = Some(error);
        let mut wakers: Vec<_> = self.wakers.drain().collect();
        wakers.sort_unstable_by_key(|&(key, _)| key);
        for (_, waker) in wakers {
            waker.wake();
        }
    }
}

/// Why this node is not the home of an index for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Away {
    /// The mesh names this other node as the home.
    Remote(types::node::Key),
    /// The mesh stopped.
    Mesh(::mesh::Stopped),
}

/// Waits until the mesh names this node the home of `index`. With no mesh, this node
/// is the home. It changes no state, so the caller checks its channels again after
/// it, then carries `index` with no `await` between.
async fn home(
    state: &Rc<RefCell<State>>,
    index: types::channel::Key,
) -> Result<(), Away> {
    let (watch, node) = {
        let state = state.borrow();
        (
            state.mesh.as_ref().map(|mesh| mesh.watch(index)),
            state.node,
        )
    };
    if let Some(mut watch) = watch {
        loop {
            match watch.next().await.map_err(Away::Mesh)? {
                Some(home) if home == node => break,
                Some(home) => return Err(Away::Remote(home)),
                None => {}
            }
        }
    }
    Ok(())
}

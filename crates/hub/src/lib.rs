//! The one path for every read and write: writer and reader sessions on the channels
//! of a shard's home.

mod channel;
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
use types::sample::{Scalar, Type};

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

/// The hub of one shard: it opens writer and reader sessions on the indexes of the
/// shard's home, and serves each hub stream of a transport session through a
/// [`Link`]. It is not `Send`: each call is on the shard's thread. Clones share it.
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
        }));
        tasks.spawn(commit::run(Rc::downgrade(&state)));
        Self(state)
    }

    /// Makes the channels of `definitions` the channels that sessions may name. It
    /// skips each definition that is not a channel. A known channel whose key, name,
    /// and definition stay keeps its sessions. Each other known channel is removed,
    /// and each session on it ends at once: [`writer::Failure::Removed`],
    /// [`reader::Ended::Removed`], and [`serve::Error::Removed`]. Then each new
    /// channel is defined. The home carries each new index at once, and stops
    /// carrying each index whose key is not an index of `definitions`.
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

    /// Opens a writer session on `config.channels` and the index of each. It opens at
    /// the first poll.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Empty`] for no name, [`writer::Error::Unknown`] for the first
    /// name that no channel has, else [`writer::Error::Home`] when the home refuses
    /// the writer.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "a remote writer will wait for its home"
    )]
    pub async fn writer(
        &self,
        config: writer::Config,
    ) -> Result<Writer, writer::Error> {
        Writer::open(&self.0, config)
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
    /// another index than the first. [`reader::Error::Empty`] for no name.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "a remote reader will wait for its home"
    )]
    pub async fn reader(
        &self,
        channels: &[Name],
        mode: reader::Mode,
    ) -> Result<Reader, reader::Error> {
        Reader::open(&self.0, channels, mode)
    }

    /// Sets the access rules that each later hello and request is checked against.
    /// Until the first call, the rules know no subject, so they refuse each hello with
    /// `access::proof::Error::Unknown`.
    pub fn set_rules(&self, rules: access::Rules) {
        self.0.borrow_mut().rules = rules;
    }

    /// The hub's part of `session`. Give each hub stream of the session to
    /// [`Link::serve`].
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

/// The channels of an open session, and the cell where the hub puts the first of
/// them that it removes. The session reads the cell, so its check costs the same
/// while other sessions end.
#[derive(Debug)]
struct Open {
    keys: Box<[Key]>,
    removed: Rc<Cell<Option<Key>>>,
}

impl<K> Default for Sessions<K> {
    fn default() -> Self {
        Self(hash::Map::default())
    }
}

impl<K: Copy + Eq + Hash> Sessions<K> {
    /// Opens the session `key` on `keys`, so that a removal of one of them ends it.
    /// Returns the cell that names the removed channel.
    fn add(&mut self, key: K, keys: Box<[Key]>) -> Rc<Cell<Option<Key>>> {
        let removed = Rc::default();
        let open = Open {
            keys,
            removed: Rc::clone(&removed),
        };
        let added = self.0.insert(key, open);
        assert!(added.is_none(), "invariant: the home gives each key once");
        removed
    }

    /// Returns whether the session `key` was open, and makes it not open.
    fn remove(&mut self, key: K) -> bool {
        self.0.remove(&key).is_some()
    }

    /// Puts the first channel of `removed` in the cell of each session on one of them.
    /// Returns their keys, to close.
    fn end(&self, removed: &hash::Set<Key>) -> Vec<K> {
        self.0
            .iter()
            .filter_map(|(&key, open)| {
                let first = open.keys.iter().find(|key| removed.contains(key));
                open.removed.set(first.copied());
                first.map(|_| key)
            })
            .collect()
    }
}

impl State {
    /// Makes `channels` the known channels, as [`Hub::set_definitions`] says.
    fn set(&mut self, channels: &hash::Map<&Name, &spec::channel::Channel>) {
        let removed: hash::Set<Key> = self
            .channels
            .iter()
            .filter(|&(name, known)| channels.get(name) != Some(&&known.definition))
            .map(|(_, known)| known.key)
            .collect();
        let index = |channel: &spec::channel::Channel| {
            matches!(channel.kind, Kind::Index { .. }).then_some(channel.key)
        };
        let before: hash::Set<Key> = self
            .channels
            .values()
            .filter_map(|known| index(&known.definition))
            .collect();
        let after: hash::Set<Key> = channels
            .values()
            .filter_map(|channel| index(channel))
            .collect();
        self.end(&removed);
        let mut shed: Vec<Key> = before.difference(&after).copied().collect();
        shed.sort_unstable();
        for key in shed {
            let slot = self.interner.slots().assign(key);
            self.home.shed(slot);
        }
        self.channels
            .retain(|_, known| !removed.contains(&known.key));
        self.indexes.retain(|key, _| !removed.contains(key));
        let mut carried: Vec<Key> = after.difference(&before).copied().collect();
        carried.sort_unstable();
        for key in carried {
            let slot = self.interner.slots().assign(key);
            self.home.carry(slot);
        }
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
        if removed.is_empty() {
            return;
        }
        for key in self.writers.end(removed) {
            self.close_writer(key);
        }
        let mut ended = self.readers.end(removed);
        ended.sort_unstable();
        for key in ended {
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
        let key = channel.key;
        let (data_type, index) = match &channel.kind {
            Kind::Index { .. } => (Type::Scalar(Scalar::Stamp), key),
            Kind::Data(data) => (data.data_type().sample(), *data.index()),
        };
        self.indexes.insert(key, index);
        let channel = Channel {
            key,
            data_type,
            index,
            definition: channel.clone(),
        };
        self.channels.insert(name.clone(), channel);
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

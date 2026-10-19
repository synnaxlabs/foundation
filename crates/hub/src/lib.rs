//! The one path for every read and write: writer and reader sessions on the channels
//! of a shard's home.

mod channel;
mod commit;
mod gate;
mod link;
mod pending;
pub mod reader;
pub mod serve;
mod task;
#[cfg(feature = "sim")]
pub mod testing;
mod time;
mod used;
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

pub use link::{Link, Served};
use reader::Reader;
pub use time::Time;
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
    /// Where the hub spawns its commit task and the task of each remote reader.
    pub tasks: env::tasks::Tasks,
    /// This node's key. A client's hello must name it as `via`.
    pub node: types::node::Key,
    /// Mesh time, which the hub checks each hello and request against, and which
    /// [`Hub::writer`] waits for: the reader that `home` was made with.
    pub time: clock::Reader,
    /// The source of each challenge's nonce.
    pub entropy: env::entropy::Entropy,
    /// The node's region, or `None` for a node with no region. With `None`, this node
    /// is the home of each index. With a region, the hub takes its definitions only
    /// from the region's mesh: the spec in use at [`Hub::new`], each later spec in use,
    /// and the spec in use at each open of a writer, a reader, or a served session,
    /// when its pointer moved. So an open works on a spec no older than each spec that
    /// the mesh gave any task before the open. A later spec ends each session on a
    /// channel that it removes, as [`Hub::set_definitions`] states, with no open.
    pub region: Option<Region>,
}

/// What the hub of a node in a region takes its definitions and homes from, and
/// reaches the homes on.
#[derive(Debug)]
pub struct Region {
    /// The region's mesh. It gives the spec in use, names the home of each index, and
    /// gives the addresses of each member.
    pub mesh: ::mesh::Mesh,
    /// The transport of this shard. The hub dials the home of each remote reader on it.
    pub transport: Rc<transport::Transport>,
}

/// The region of a hub, and the spec in use that the hub applied last.
#[derive(Debug)]
struct Regional {
    region: Region,
    applied: used::Applied,
}

/// The state of one shard's hub, which each session shares. No borrow of it lasts
/// past a call or across an `.await`.
#[derive(Debug)]
struct State {
    home: ::home::Shard,
    interner: Interner,
    channels: channel::Table,
    /// Each open writer, by its home key.
    writers: Sessions<::home::writer::Key>,
    readers: Sessions<::home::reader::Key>,
    remotes: reader::remote::Sessions,
    /// The waker of each reader that waits for a frame.
    wakers: hash::Map<::home::reader::Key, Waker>,
    /// The channels of each open that waits, by a key from `opened`.
    opens: Sessions<pending::Key>,
    /// The waker of each open in `opens`.
    waiting: hash::Map<pending::Key, Waker>,
    /// The count of opens ever registered in `opens`.
    opened: u64,
    /// The readers that [`::home::Shard::woken`] gave last.
    woken: Vec<::home::reader::Key>,
    commit: commit::Task,
    /// The gate task, woken at each new carry, each writer open and close, and each
    /// named complete open: the calls that make the home's deadline earlier by one
    /// second or more.
    gate: task::Waiter,
    /// The error that ended the home's buffer.
    failed: Option<env::files::Error>,
    node: types::node::Key,
    time: clock::Reader,
    entropy: env::entropy::Entropy,
    /// Empty, so refusing each hello, until [`Hub::set_rules`] first runs.
    rules: access::Rules,
    region: Option<Regional>,
    /// Where the hub spawns the task of each remote reader.
    tasks: env::tasks::Tasks,
    bodies: serve::client::Bodies,
}

impl Hub {
    /// A hub over `config.home` that knows each channel of the spec that the mesh of
    /// `config.region` uses, or no channel with no region. Spawns two tasks on
    /// `config.tasks`: one that ends when the home's buffer fails, and one that applies
    /// the end of each control lease and grace at the home. With a region, it spawns a
    /// third, which applies each later spec in use ([`Config::region`]). The hub holds
    /// the home and `config.region` until the hub and each value and future that it
    /// gave, directly or through another such value, drop. Each task of the hub ends at
    /// its next poll after that, and then holds neither. So a `::home::Commit` taken
    /// before `new` and awaited after that drop resolves once the buffer's task ended,
    /// and when the caller holds no other part of the home, the ring closes when that
    /// commit drops.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let Config {
            home,
            interner,
            tasks,
            node,
            time,
            entropy,
            region,
        } = config;
        let next = home.next_commit();
        let (region, used) = region
            .map(|region| {
                let used = region.mesh.used();
                let applied = used::Applied::new(region.mesh.watch_spec(), &used);
                (Regional { region, applied }, used)
            })
            .unzip();
        let state = Rc::new(RefCell::new(State {
            home,
            interner,
            channels: channel::Table::default(),
            writers: Sessions::default(),
            readers: Sessions::default(),
            remotes: reader::remote::Sessions::default(),
            wakers: hash::Map::default(),
            opens: Sessions::default(),
            waiting: hash::Map::default(),
            opened: 0,
            woken: Vec::new(),
            commit: commit::Task::new(next),
            gate: task::Waiter::default(),
            failed: None,
            node,
            time: time.clone(),
            entropy,
            rules: access::Rules::default(),
            region,
            tasks: tasks.clone(),
            bodies: serve::client::Bodies::default(),
        }));
        tasks.spawn(commit::run(Rc::downgrade(&state)));
        tasks.spawn(gate::run(Rc::downgrade(&state), time));
        if let Some(used) = used {
            state.borrow_mut().define(&*used.definitions);
            tasks.spawn(used::run(Rc::downgrade(&state)));
        }
        Self(state)
    }

    /// Makes the channels of `definitions`, in any order, the channels that sessions
    /// may name. It skips each definition that is not a channel. A known channel whose
    /// key, name, and definition stay keeps its sessions. Each other known channel is
    /// removed, and each session on it ends at once: [`writer::Failure::Removed`],
    /// [`reader::Ended::Removed`], and [`serve::Error::Removed`]. A write stream ends
    /// only the group of the channel, with [`wire::hub::write::Outcome::Ended`] of
    /// [`wire::hub::write::End::Removed`]. Then each new channel is defined. The home
    /// stops carrying each index whose key is not an index of `definitions`, and
    /// carries an index from the first session that finds this node is its home. A
    /// reader of a data channel takes each series of its key with its sample type in
    /// the frames of its index, also one written before a removal of the channel while
    /// its index stayed, so a rename keeps the history of the channel.
    ///
    /// # Panics
    ///
    /// If the hub has a region: its definitions come from the region's mesh. Before
    /// any change, when two channels of `definitions` have one key or one name, or the
    /// index of a data channel is not an index of `definitions`.
    pub fn set_definitions<'d>(
        &self,
        definitions: impl IntoIterator<Item = (&'d Name, &'d Definition)>,
    ) {
        let mut state = self.0.borrow_mut();
        assert!(
            state.region.is_none(),
            "a hub with a region takes its definitions from the region's mesh"
        );
        state.define(definitions);
    }

    /// The node's mesh time. Waits for the node's first mesh time, as the open of a
    /// writer does, so the [`Time`] it gives always has a stamp. Dropping the future
    /// ends the wait.
    pub async fn time(&self) -> Time {
        time::known(&self.0).await;
        Time(Rc::clone(&self.0))
    }

    /// Opens a writer session on `config.channels` and the index of each. It waits
    /// for the node's first mesh time, and while the mesh names no home for an index,
    /// for one. A later spec in use that removes or changes one of its channels ends
    /// that wait, and it checks its names again.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Empty`] for no name, [`writer::Error::Unknown`] for the first
    /// name that no channel has, then, for the first index whose home is not this
    /// node, [`writer::Error::Remote`], or [`writer::Error::Mesh`] when the mesh
    /// stopped. Else [`writer::Error::Home`] for a lease that is not longer than zero:
    /// never `Unsynced`.
    pub async fn writer(
        &self,
        config: writer::Config,
    ) -> Result<Writer, writer::Error> {
        let writer = writer::Homed::new(&self.0, config).await?.open()?;
        Ok(writer)
    }

    /// Opens a reader session on the channels that `config.select` matches, which share
    /// one index. While the mesh names no home for the index, it waits for one. A later
    /// spec in use that removes or changes one of its channels ends that wait, and it
    /// matches the selector again. At the home of another node, each open reader holds
    /// one stream of the one session to it, so it also waits while that home allows
    /// this node one more stream or none, as the last one is for the mesh, until
    /// another stream there ends. It gets each frame of the index, as a view of only
    /// its channels and their index. An unnamed complete reader gets each live frame
    /// written after the returned future resolves, with the seqs trimmed or lost for
    /// no room as the gap of the next frame with a sample ([`reader::Mode::Complete`]).
    /// A named reader takes over the open session of its subject and name, which ends
    /// with [`reader::Ended::Replaced`], and a named complete one resumes as
    /// [`reader::Config::name`] says.
    ///
    /// # Errors
    ///
    /// [`reader::Error::Empty`] when the selector matches no channel, and
    /// [`reader::Error::ManyIndexes`] when the channels are on more than one index.
    /// Then [`reader::Error::Mesh`] when the mesh stopped. A named reader:
    /// [`reader::Error::Remote`] when the home is another node,
    /// [`reader::Error::Unsynced`] before the node has mesh time, and, when complete,
    /// [`reader::Error::Full`] when it holds no record and the named records of the
    /// index have no room for one, and
    /// [`reader::Error::Unrecorded`] when the home had no room to record the reader's
    /// position. At the home of another node:
    /// [`reader::Error::Transport`] when the dial or the stream fails,
    /// [`reader::Error::Refused`] when the home refuses the session,
    /// [`reader::Error::Message`] for a reply that breaks the hub protocol, and
    /// [`reader::Error::Pool`] when the shard's pool has no block for the open.
    ///
    /// # Panics
    ///
    /// When `config.hold` is negative, or not zero for an unnamed or latest reader.
    pub async fn reader(
        &self,
        config: reader::Config,
    ) -> Result<Reader, reader::Error> {
        Reader::open(&self.0, config).await
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

/// The open sessions of one kind, by key, with the channels of each. A session is
/// open while it is here.
#[derive(Debug)]
struct Sessions<K>(hash::Map<K, Open>);

/// The channels of an open session, and its ending.
#[derive(Debug)]
struct Open {
    keys: Vec<Key>,
    ending: Ending,
}

/// Why the hub ended a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The hub removed this channel of the session, its first one removed.
    Removed(Key),
    /// A later open of the same named reader took over the session.
    Replaced,
}

/// Why the hub ended a session, which the session reads at each call. It reads no map,
/// so its check costs the same while other sessions end.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ending(Rc<Cell<Option<End>>>);

impl Ending {
    /// Why the hub ended the session, or `None` while it is open.
    pub(crate) fn get(&self) -> Option<End> {
        self.0.get()
    }

    /// The channel whose removal ended the session, or `None` while it is open.
    ///
    /// # Panics
    ///
    /// When a takeover ended it: only the session of a named reader has one.
    pub(crate) fn removed(&self) -> Option<Key> {
        self.get().map(|end| match end {
            End::Removed(key) => key,
            End::Replaced => panic!("invariant: only a named reader is taken over"),
        })
    }
}

impl<K> Default for Sessions<K> {
    fn default() -> Self {
        Self(hash::Map::default())
    }
}

impl<K: Copy + Ord + Hash> Sessions<K> {
    /// Opens the session `key` on `keys`, so that a removal of one of them ends it.
    fn add(&mut self, key: K, keys: Vec<Key>) -> Ending {
        let ending = Ending::default();
        let open = Open {
            keys,
            ending: ending.clone(),
        };
        let added = self.0.insert(key, open);
        assert!(added.is_none(), "invariant: each key is added once");
        ending
    }

    /// Returns whether the session `key` was open, and makes it not open.
    fn remove(&mut self, key: K) -> bool {
        self.0.remove(&key).is_some()
    }

    /// Ends each session on a channel of `removed` with the first such channel.
    /// Returns their keys in order, to close.
    fn end(&self, removed: &hash::Set<Key>) -> Vec<K> {
        let mut ended: Vec<K> = self
            .0
            .iter()
            .filter_map(|(&key, open)| {
                let first = open.keys.iter().find(|key| removed.contains(key))?;
                open.ending.0.set(Some(End::Removed(*first)));
                Some(key)
            })
            .collect();
        ended.sort_unstable();
        ended
    }
}

impl Sessions<pending::Key> {
    /// Adds `keys` to the channels of the open `key`.
    ///
    /// # Panics
    ///
    /// When `key` is not open.
    fn extend(&mut self, key: pending::Key, keys: &[Key]) {
        let open = self
            .0
            .get_mut(&key)
            .expect("invariant: the open is registered");
        open.keys.extend_from_slice(keys);
    }
}

impl Sessions<::home::reader::Key> {
    /// Makes the open reader session `key` not open, and ends it with
    /// [`End::Replaced`].
    fn replace(&mut self, key: ::home::reader::Key) {
        let open = self.0.remove(&key).expect("invariant: the session is open");
        open.ending.0.set(Some(End::Replaced));
    }
}

impl State {
    /// Makes the channels of `definitions` the defined channels, as
    /// [`Hub::set_definitions`] says.
    fn define<'d>(
        &mut self,
        definitions: impl IntoIterator<Item = (&'d Name, &'d Definition)>,
    ) {
        let channels: Vec<_> = definitions
            .into_iter()
            .filter_map(|(name, definition)| match definition {
                Definition::Channel(channel) => Some((name, channel)),
                _ => None,
            })
            .collect();
        let removed = self.channels.set(&checked(&channels));
        self.end(&removed.channels);
        for key in removed.indexes {
            let slot = self.interner.slots().index(key);
            self.home.shed(slot);
        }
    }

    /// Applies the spec that the region's mesh uses now. Does nothing with no region.
    fn catch_up(&mut self) {
        if let Some(regional) = &mut self.region {
            let spec = regional.region.mesh.used();
            if regional.applied.advance(&spec) {
                self.define(&*spec.definitions);
            }
        }
    }

    /// Ends each session on a channel of `removed`, and closes each local one at the
    /// home.
    fn end(&mut self, removed: &hash::Set<Key>) {
        for key in self.writers.end(removed) {
            self.close_writer(key);
        }
        for key in self.readers.end(removed) {
            if let Some(waker) = self.close_reader(key) {
                waker.wake();
            }
        }
        for key in self.opens.end(removed) {
            self.opens.remove(key);
            if let Some(waker) = self.waiting.remove(&key) {
                waker.wake();
            }
        }
        self.remotes.end(removed);
    }

    /// Closes the writer `key` at the home, unless a removal closed it.
    fn close_writer(&mut self, key: ::home::writer::Key) {
        if self.writers.remove(key) {
            self.home.close_writer(key);
            self.gate.wake();
        }
    }

    /// Closes the reader session `key` at the home, unless a removal or a takeover
    /// closed it. Returns its waker, when it waits for a frame.
    fn close_reader(&mut self, key: ::home::reader::Key) -> Option<Waker> {
        let waker = self.wakers.remove(&key);
        if self.readers.remove(key) {
            self.home.close_reader(key);
        }
        waker
    }

    /// Ends the session that `opened` took over, which the home closed, with
    /// [`reader::Ended::Replaced`]. Returns the key of the new session.
    fn take_over<K>(&mut self, opened: ::home::reader::Opened<K>) -> K {
        if let Some(key) = opened.replaced {
            self.readers.replace(key);
            if let Some(waker) = self.wakers.remove(&key) {
                waker.wake();
            }
        }
        opened.key
    }

    /// A block of `len` bytes from the home's pool.
    ///
    /// # Errors
    ///
    /// [`block::Error::Exhausted`] or [`block::Error::Refused`] when the pool has no
    /// block for it now.
    ///
    /// # Panics
    ///
    /// When `len` is over the pool's largest block: each caller asks for at most that.
    fn alloc(&self, len: usize) -> Result<block::Unique, block::Error> {
        self.home.pool().alloc(len).map_err(|error| match error {
            block::Error::TooLarge { .. } => {
                unreachable!(
                    "invariant: no caller asks for more than the largest block"
                )
            }
            block::Error::Exhausted { .. } | block::Error::Refused { .. } => error,
        })
    }

    /// Carries `index` at the home. Wakes the gate task when the carry is new: only
    /// then can it make the home's deadline earlier.
    fn carry(&mut self, index: types::channel::Key) {
        let slot = self.interner.slots().index(index);
        if self.home.carry(slot, index) {
            self.gate.wake();
        }
    }

    /// Carries `index` at the home, and gives the slot of each of `keys` in its role:
    /// `index` as an index, and each other key as a data channel of its defined
    /// sample type.
    ///
    /// # Panics
    ///
    /// When a key of `keys` is not defined.
    fn slots(&mut self, index: Key, keys: &[Key]) -> Box<[types::channel::Slot]> {
        self.carry(index);
        let assigned = self.interner.slots();
        let slot = assigned.index(index);
        keys.iter()
            .map(|&key| {
                if key == index {
                    return slot;
                }
                assigned.data(key, self.channels.known(key).sample())
            })
            .collect()
    }

    /// Wakes each reader that the home names as having a frame to take, a read from
    /// disk to run, or a lost frame to report.
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

/// Waits until the mesh names this node the home of each of `indexes`, in one pass
/// that waits for none, so a home that moves while it waits for another is seen. With
/// no mesh, this node is the home. It changes no state, so the caller checks its
/// channels again, or reads their removal, after it, then carries `indexes` with no
/// `await` between.
async fn homes(
    state: &Rc<RefCell<State>>,
    indexes: &[types::channel::Key],
) -> Result<(), Away> {
    loop {
        let mut waited = false;
        for &index in indexes {
            let (watch, node) = {
                let state = state.borrow();
                (
                    state
                        .region
                        .as_ref()
                        .map(|regional| regional.region.mesh.watch(index)),
                    state.node,
                )
            };
            let Some(mut watch) = watch else {
                return Ok(());
            };
            loop {
                match watch.next().await.map_err(Away::Mesh)? {
                    Some(home) if home == node => break,
                    Some(home) => return Err(Away::Remote(home)),
                    None => waited = true,
                }
            }
        }
        if !waited {
            return Ok(());
        }
    }
}

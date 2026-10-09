//! The one path for every read and write: writer and reader sessions on the channels
//! of a shard's home.

mod channel;
pub mod client;
mod commit;
mod link;
pub mod reader;
pub mod serve;
pub mod writer;

use std::cell::RefCell;
use std::rc::Rc;
use std::task::Waker;

use spec::channel::Kind;
use spec::definition::Definition;
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

/// The `mesh` items that hub calls give, so that layer 3 names them through `hub`.
pub mod mesh {
    pub use ::mesh::Stopped;
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
    indexes: hash::Map<types::channel::Key, types::channel::Key>,
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

    /// Makes each channel of `definitions` known to sessions, the indexes first, so
    /// their order does not matter. It skips each definition that is not a channel. The
    /// home carries an index from the first session that finds this node is its home.
    ///
    /// # Panics
    ///
    /// When a channel has the key or name of a known channel or of another channel of
    /// `definitions`, or the index of a data channel is neither known nor an index of
    /// `definitions`.
    pub fn define<'d>(
        &self,
        definitions: impl IntoIterator<Item = (&'d Name, &'d Definition)>,
    ) {
        let (indexes, data): (Vec<_>, Vec<_>) = definitions
            .into_iter()
            .filter_map(|(name, definition)| match definition {
                Definition::Channel(channel) => Some((name, channel)),
                _ => None,
            })
            .partition(|(_, channel)| matches!(channel.kind, Kind::Index { .. }));
        let mut state = self.0.borrow_mut();
        for (name, channel) in indexes.into_iter().chain(data) {
            state.define(name, channel);
        }
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

    /// The hub's part of `session`. Give each hub stream of the session to
    /// [`Link::serve`].
    #[must_use]
    pub fn link(&self, session: transport::Session) -> Link {
        Link::new(Rc::clone(&self.0), session)
    }
}

impl State {
    /// Makes `channel` known to sessions as `name`, with the panics of
    /// [`Hub::define`].
    fn define(&mut self, name: &Name, channel: &spec::channel::Channel) {
        let key = channel.key;
        assert!(
            !self.indexes.contains_key(&key) && !self.channels.contains_key(name),
            "a channel with key {key} or name {name} is known already"
        );
        let (data_type, index) = match &channel.kind {
            Kind::Index { .. } => (Type::Scalar(Scalar::Stamp), key),
            Kind::Data(data) => {
                let index = *data.index();
                assert!(
                    self.indexes.get(&index) == Some(&index),
                    "the index {index} of channel {name} is not a known index"
                );
                (data.data_type().sample(), index)
            }
        };
        self.indexes.insert(key, index);
        let channel = Channel {
            key,
            data_type,
            index,
        };
        self.channels.insert(name.clone(), channel);
    }

    /// Carries `index` at the home. A later carry does nothing.
    fn carry(&mut self, index: types::channel::Key) {
        let slot = self.interner.slots().assign(index);
        self.home.carry(slot);
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

/// Why the home did not carry an index for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Away {
    /// The mesh names this other node as the home.
    Remote(types::node::Key),
    /// The mesh stopped.
    Mesh(::mesh::Stopped),
}

/// Waits until the mesh names a home for `index`, then carries `index` at the home
/// when the home is this node. With no mesh, this node is the home.
async fn carry(
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
    state.borrow_mut().carry(index);
    Ok(())
}

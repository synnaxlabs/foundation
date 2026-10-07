//! The one path for every read and write: sessions across homes, routing, live
//! selectors, the server loop, authentication, encode and decode once, raw cursors for
//! replicas, re-index stitching, and the layer-3 window.

mod channel;
mod commit;
pub mod reader;
pub mod writer;

use std::cell::RefCell;
use std::future;
use std::rc::Rc;
use std::task::Waker;

use types::frame::key_set::Interner;
use types::hash;
use types::name::Name;

pub use channel::Channel;
pub use reader::Reader;
pub use writer::Writer;

/// The `home` items that hub calls give, so that layer 3 names them through `hub`.
pub mod home {
    pub use ::home::{Error, Outcome, Refusal, order};

    /// Why the home refused a writer.
    pub mod writer {
        pub use ::home::writer::Error;
    }
}

/// The hub of one shard: it opens writer and reader sessions on the indexes of the
/// shard's home. It is not `Send`: each call is on the shard's thread. Clones share
/// it.
#[derive(Clone, Debug)]
pub struct Hub(Rc<RefCell<State>>);

/// What one shard's hub is given at start.
#[derive(Debug)]
pub struct Config {
    /// The shard's home. Only the hub calls it.
    pub home: ::home::Shard,
    /// The node's key set interner, which owns the slot table that the home's buffer
    /// opened with. The node runs one shard; interning across shards waits for a
    /// second shard.
    pub interner: Interner,
    /// The pool of the home's buffer, for the frames that writers fill.
    pub pool: Rc<block::Pool>,
    /// Where the hub spawns its commit task.
    pub tasks: env::tasks::Tasks,
}

/// The state of one shard's hub, which each session shares. No borrow of it lasts
/// past a call or across an `.await`.
#[derive(Debug)]
struct State {
    home: ::home::Shard,
    interner: Interner,
    pool: Rc<block::Pool>,
    channels: hash::Map<Name, Channel>,
    /// The waker of each reader that waits for a frame.
    wakers: hash::Map<::home::reader::Key, Waker>,
    /// The readers that [`::home::Shard::woken`] gave last.
    woken: Vec<::home::reader::Key>,
    commit: commit::State,
    /// The error that ended the home's buffer.
    failed: Option<env::files::Error>,
}

impl Hub {
    /// A hub over `config.home` that knows no channel yet. Spawns the commit task on
    /// `config.tasks`. The task holds the hub's state until the shard stops.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let Config {
            home,
            interner,
            pool,
            tasks,
        } = config;
        let state = Rc::new(RefCell::new(State {
            home,
            interner,
            pool,
            channels: hash::Map::default(),
            wakers: hash::Map::default(),
            woken: Vec::new(),
            commit: commit::State::default(),
            failed: None,
        }));
        tasks.spawn(commit::run(Rc::clone(&state)));
        Self(state)
    }

    /// Makes `channel` known to sessions. The home carries an index at once.
    ///
    /// # Panics
    ///
    /// If a channel with the same key or name is known, or the index of a data
    /// channel is not a known index.
    pub fn define(&self, channel: Channel) {
        let mut state = self.0.borrow_mut();
        let state = &mut *state;
        let known = state.channels.values().find(|c| c.key == channel.key);
        assert!(
            known.is_none() && !state.channels.contains_key(&channel.name),
            "a channel with key {} or name {} is known already",
            channel.key,
            channel.name
        );
        if channel.index == channel.key {
            let slot = state.interner.slots().assign(channel.key);
            state.home.carry(slot);
        } else {
            let index = state.channels.values().find(|c| c.key == channel.index);
            assert!(
                index.is_some_and(|index| index.index == index.key),
                "the index {} of channel {} is not a known index",
                channel.index,
                channel.name
            );
        }
        state.channels.insert(channel.name.clone(), channel);
    }

    /// Opens a writer session on `config.channels` and the index of each. A local
    /// writer opens in the call, and the future is ready; a remote writer will wait
    /// for its home.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Unknown`] for the first name that no channel has, then
    /// [`writer::Error::Home`] when the home refuses the writer.
    pub fn writer(
        &self,
        config: writer::Config,
    ) -> impl Future<Output = Result<Writer, writer::Error>> {
        future::ready(Writer::open(&self.0, config))
    }

    /// Opens a reader session on `channels`, which share one index, as
    /// [`writer`](Self::writer) opens a writer. It gets each frame of the index, with
    /// every channel its writer wrote. A complete reader gets each live frame written
    /// after this call.
    ///
    /// # Errors
    ///
    /// For the first name that breaks a rule: [`reader::Error::Unknown`] for a name
    /// that no channel has, and [`reader::Error::ManyIndexes`] for a channel on
    /// another index than the first. [`reader::Error::Empty`] for no name.
    pub fn reader(
        &self,
        channels: &[Name],
        mode: reader::Mode,
    ) -> impl Future<Output = Result<Reader, reader::Error>> {
        future::ready(Reader::open(&self.0, channels, mode))
    }
}

impl State {
    /// Wakes each reader that the home names as having a frame to take.
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

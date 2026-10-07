//! The one path for every read and write: writer and reader sessions on the channels
//! of a shard's home.

mod channel;
mod commit;
pub mod reader;
pub mod writer;

use std::cell::RefCell;
use std::rc::Rc;
use std::task::Waker;

use types::frame::key_set::Interner;
use types::hash;
use types::name::Name;

pub use channel::Channel;
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
    /// opened with.
    pub interner: Interner,
    /// Where the hub spawns its commit task.
    pub tasks: env::tasks::Tasks,
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
}

impl Hub {
    /// A hub over `config.home` that knows no channel yet. Spawns a task on
    /// `config.tasks` that ends when the home's buffer fails, or once the hub and each
    /// of its sessions have dropped. Once the hub and each of its sessions drop, it
    /// holds no part of the home. So a `home::Commit` taken before `new` and awaited
    /// after that drop resolves once the buffer's task ended, and when the caller holds
    /// no other part of the home, the ring closes when that commit drops.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let Config {
            home,
            interner,
            tasks,
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
        }));
        tasks.spawn(commit::run(Rc::downgrade(&state)));
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
        assert!(
            !state.indexes.contains_key(&channel.key)
                && !state.channels.contains_key(&channel.name),
            "a channel with key {} or name {} is known already",
            channel.key,
            channel.name
        );
        if channel.index == channel.key {
            let slot = state.interner.slots().assign(channel.key);
            state.home.carry(slot);
        } else {
            assert!(
                state.indexes.get(&channel.index) == Some(&channel.index),
                "the index {} of channel {} is not a known index",
                channel.index,
                channel.name
            );
        }
        state.indexes.insert(channel.key, channel.index);
        state.channels.insert(channel.name.clone(), channel);
    }

    /// Opens a writer session on `config.channels` and the index of each. It opens at
    /// the first poll.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Empty`] for no name, [`writer::Error::Unknown`] for the first
    /// name that no channel has, then [`writer::Error::Type`] for a channel of a type
    /// that the home does not write, else [`writer::Error::Home`] when the home
    /// refuses the writer.
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
}

impl State {
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

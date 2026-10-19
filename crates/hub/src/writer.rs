//! Writer sessions: what one opens with, why one does not open, and the session.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use types::authority::Authority;
use types::channel;
use types::frame::key_set::{Group, KeySet};
use types::frame::{self, Draft, Form, Label};
use types::hash;
use types::name::Name;
use types::sample::Type;
use types::time::{Span, Stamp};

use crate::pending::Pending;
use crate::time::now;
use crate::{Away, Ending, State, Time};

/// What a writer session opens with.
#[derive(Clone, Debug)]
pub struct Config {
    /// The subject that opens the writer.
    pub subject: Name,
    /// The writer's authority. Nothing caps it by access yet.
    pub authority: Authority,
    /// How long the writer may go without a write and keep control, or `None` for no
    /// limit.
    pub lease: Option<Span>,
    /// The channels the writer writes. The hub adds the index of each.
    pub channels: Vec<Name>,
}

/// Why a writer session did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No channel has this name.
    Unknown(Name),
    /// The home refused the writer.
    Home(::home::writer::Error),
    /// The writer names no channel.
    Empty,
    /// The home of an index of the writer is `home`, another node. A writer writes
    /// only at the home of each of its indexes.
    Remote {
        /// The home.
        home: types::node::Key,
    },
    /// The mesh stopped, so the home of an index is not known.
    Mesh(mesh::Stopped),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no channel is named {name}"),
            Self::Home(error) => error.fmt(f),
            Self::Empty => f.write_str("a writer names at least one channel"),
            Self::Remote { home } => write!(
                f,
                "the home of an index of the writer is node {home}, and a writer \
                 writes only at this node"
            ),
            Self::Mesh(stopped) => write!(f, "the mesh stopped: {stopped}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Away> for Error {
    fn from(away: Away) -> Self {
        match away {
            Away::Remote(home) => Self::Remote { home },
            Away::Mesh(stopped) => Self::Mesh(stopped),
        }
    }
}

/// Why a write failed. No seq moves for either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The home refused the frame.
    Home(::home::Error),
    /// A channel of the writer was removed from the definitions. The writer takes no
    /// more frames: open a new writer.
    Removed(channel::Key),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Home(error) => error.fmt(f),
            Self::Removed(key) => {
                write!(f, "channel {key} was removed: open a new writer")
            }
        }
    }
}

impl std::error::Error for Failure {}

/// An index, and the key and sample type of each data channel of a writer on it.
type Indexed = (channel::Key, Vec<(channel::Key, Type)>);

/// The key of each of `channels`, and their data channels by index, in the order of
/// `channels`, after the spec in use applies.
fn resolve(
    state: &mut State,
    channels: &[Name],
) -> Result<(Vec<channel::Key>, Vec<Indexed>), Error> {
    state.catch_up();
    let mut groups: Vec<Indexed> = Vec::new();
    let mut keys = Vec::with_capacity(channels.len());
    let mut positions = hash::Map::default();
    let mut data = hash::Set::default();
    for name in channels {
        let channel = state
            .channels
            .named(name)
            .ok_or_else(|| Error::Unknown(name.clone()))?;
        let (key, index) = (channel.key(), channel.index());
        keys.push(key);
        let at = *positions.entry(index).or_insert_with(|| {
            groups.push((index, Vec::new()));
            groups.len() - 1
        });
        if key != index && data.insert(key) {
            groups[at].1.push((key, channel.sample()));
        }
    }
    Ok((keys, groups))
}

/// The entry in `set` of each of `keys`, in their order.
fn entries(set: &KeySet, keys: &[channel::Key]) -> Box<[usize]> {
    let entry_of: hash::Map<channel::Key, usize> = set
        .entries()
        .iter()
        .enumerate()
        .map(|(at, entry)| (entry.key, at))
        .collect();
    keys.iter()
        .map(|key| {
            let entry = entry_of.get(key).copied();
            entry
                .unwrap_or_else(|| panic!("invariant: the key set holds channel {key}"))
        })
        .collect()
}

/// A writer session. Dropping it closes the session.
#[derive(Debug)]
pub struct Writer {
    state: Rc<RefCell<State>>,
    key: ::home::writer::Key,
    /// Why the hub ended the writer.
    ending: Ending,
    set: Arc<KeySet>,
    /// The entry in `set` of each channel of the config, in its order.
    entries: Box<[usize]>,
    /// The outcomes of the last write.
    outcomes: Vec<::home::Outcome>,
}

/// A writer whose channels resolved, with this node the home of each index. It opens
/// with no wait.
pub(crate) struct Homed {
    state: Rc<RefCell<State>>,
    subject: Name,
    authority: Authority,
    lease: Option<Span>,
    keys: Vec<channel::Key>,
    groups: Vec<Indexed>,
    channels: Vec<Name>,
}

impl Homed {
    /// Resolves `config.channels`, and waits for mesh time and for this node to be
    /// the home of each index.
    pub(crate) async fn new(
        state: &Rc<RefCell<State>>,
        config: Config,
    ) -> Result<Self, Error> {
        let homed = Self::within(state, config, usize::MAX).await?;
        Ok(homed.expect("invariant: no writer has more than usize::MAX indexes"))
    }

    /// As [`Homed::new`] for a writer on one index. Gives `None` when its channels
    /// resolve to more than one index: before the wait for the home, or when a change
    /// of a channel during that wait gives them a second.
    pub(crate) async fn one(
        state: &Rc<RefCell<State>>,
        config: Config,
    ) -> Result<Option<Self>, Error> {
        Self::within(state, config, 1).await
    }

    /// As [`Homed::new`], or `None` when the channels resolve to more than `indexes`
    /// indexes after the wait for mesh time.
    async fn within(
        state: &Rc<RefCell<State>>,
        config: Config,
        indexes: usize,
    ) -> Result<Option<Self>, Error> {
        let Config {
            subject,
            authority,
            lease,
            channels,
        } = config;
        if channels.is_empty() {
            return Err(Error::Empty);
        }
        // An unknown name fails before the wait for mesh time.
        resolve(&mut state.borrow_mut(), &channels)?;
        crate::time::known(state).await;
        let (keys, groups) = loop {
            let (keys, groups) = resolve(&mut state.borrow_mut(), &channels)?;
            if groups.len() > indexes {
                return Ok(None);
            }
            let checked: Vec<channel::Key> =
                groups.iter().map(|&(index, _)| index).collect();
            let pending = Pending::new(state, keys.clone());
            // A change of a channel ends the wait at once, and the names resolve again.
            if let Ok(homed) = pending.wait(crate::homes(state, &checked)).await {
                homed?;
                break (keys, groups);
            }
        };
        Ok(Some(Self {
            state: Rc::clone(state),
            subject,
            authority,
            lease,
            keys,
            groups,
            channels,
        }))
    }

    /// Opens the writer at the home, and gives it with the channels of its config.
    /// Call it with no wait after [`Homed::new`] or [`Homed::one`], so that the
    /// channels and their homes are as it found them.
    pub(crate) fn open(self) -> Result<(Writer, Vec<Name>), Error> {
        let Self {
            state,
            subject,
            authority,
            lease,
            mut keys,
            groups,
            channels,
        } = self;
        let mut borrowed = state.borrow_mut();
        let borrowed = &mut *borrowed;
        for &(index, _) in &groups {
            borrowed.carry(index);
        }
        let groups: Vec<_> = groups
            .iter()
            .map(|(index, data)| Group {
                index: *index,
                data,
            })
            .collect();
        let set = borrowed.interner.intern(&groups);
        let entries = entries(&set, &keys);
        let writer = ::home::writer::Writer {
            subject,
            authority,
            via: borrowed.node,
            lease,
            set: Arc::clone(&set),
        };
        let key = borrowed.home.open_writer(writer).map_err(Error::Home)?;
        borrowed.gate.wake();
        keys.extend(groups.iter().map(|group| group.index));
        let ending = borrowed.writers.add(key, keys);
        let writer = Writer {
            state: Rc::clone(&state),
            key,
            ending,
            set,
            entries,
            outcomes: Vec::new(),
        };
        Ok((writer, channels))
    }
}

impl Writer {
    /// The key set of every frame the writer writes.
    #[must_use]
    pub fn set(&self) -> &Arc<KeySet> {
        &self.set
    }

    /// The entry of each channel of [`Config::channels`], in that order: its
    /// position in the entries of [`Self::set`], as [`Self::draft`] and
    /// [`Draft::series_mut`] take it. A channel named twice has the same entry twice.
    #[must_use]
    pub fn entries(&self) -> &[usize] {
        &self.entries
    }

    /// Mesh time now, as the home stamps each entry and checks each stamp: it never
    /// goes back, and two calls can give the same stamp. Each stamp of a path must be
    /// after the one before it
    /// ([`order::Error::Backwards`](crate::home::order::Error::Backwards)).
    #[must_use]
    pub fn now(&self) -> Stamp {
        now(&self.state)
    }

    /// The mesh time of the writer's hub, as [`Writer::now`] gives it, also after the
    /// writer drops.
    #[must_use]
    pub fn time(&self) -> Time {
        Time(Rc::clone(&self.state))
    }

    /// A frame of the writer's key set to fill, from the shard's pool, as
    /// [`Draft::new`] takes `series`.
    ///
    /// # Errors
    ///
    /// As [`Draft::new`].
    pub fn draft(
        &self,
        form: Form,
        series: &[(usize, usize)],
    ) -> Result<Draft, frame::Error> {
        Draft::new(self.state.borrow().home.pool(), &self.set, form, series)
    }

    /// Applies `frame` to each index it holds, whole or not at all per index, and wakes
    /// each reader that now has a frame to take. Does not wait for the commit. Returns
    /// the outcome of each present group, in group order: applied, lost (a live group
    /// with no room, whose seq is a gap), or refused (no seq spent). The slice lives
    /// until the next write.
    ///
    /// # Errors
    ///
    /// [`Failure::Removed`] once a channel of the writer is removed, on this and every
    /// later call. Else [`Failure::Home`] with the home's error.
    /// [`Error::Resend`](crate::home::Error::Resend) for a frame labeled resend.
    /// [`Error::Full`](crate::home::Error::Full) for a backfill frame with no room:
    /// write it again on a timer.
    /// [`Error::Large`](crate::home::Error::Large) for a frame too large for one write:
    /// split it. [`Error::Disk`](crate::home::Error::Disk) after a failed commit: the
    /// shard takes no more frames.
    ///
    /// # Panics
    ///
    /// If `frame` is not of [`Self::set`] and is not labeled resend, and no channel of
    /// the writer is removed.
    pub fn write(
        &mut self,
        label: Label,
        frame: Draft,
    ) -> Result<&[::home::Outcome], Failure> {
        if let Some(key) = self.ending.removed() {
            return Err(Failure::Removed(key));
        }
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let written = state.home.write(self.key, label, frame);
        let outcomes = written.map_err(Failure::Home)?;
        self.outcomes.clear();
        self.outcomes.extend_from_slice(outcomes);
        state.wake();
        Ok(&self.outcomes)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.state.borrow_mut().close_writer(self.key);
    }
}

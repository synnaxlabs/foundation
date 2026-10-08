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
use types::time::Span;

use crate::{Located, State};

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

/// A writer session. Dropping it closes the session.
#[derive(Debug)]
pub struct Writer {
    state: Rc<RefCell<State>>,
    key: ::home::writer::Key,
    set: Arc<KeySet>,
    /// The outcomes of the last write.
    outcomes: Vec<::home::Outcome>,
}

impl Writer {
    /// Opens a writer on `config.channels` and their indexes.
    pub(crate) async fn open(
        state: &Rc<RefCell<State>>,
        config: Config,
    ) -> Result<Self, Error> {
        let Config {
            subject,
            authority,
            lease,
            channels,
        } = config;
        if channels.is_empty() {
            return Err(Error::Empty);
        }
        let mut groups: Vec<(channel::Key, Vec<(channel::Key, Type)>)> = Vec::new();
        {
            let borrowed = state.borrow();
            let mut positions = hash::Map::default();
            let mut data = hash::Set::default();
            for name in &channels {
                let channel = borrowed
                    .channels
                    .get(name)
                    .ok_or_else(|| Error::Unknown(name.clone()))?;
                let at = *positions.entry(channel.index).or_insert_with(|| {
                    groups.push((channel.index, Vec::new()));
                    groups.len() - 1
                });
                if channel.key != channel.index && data.insert(channel.key) {
                    groups[at].1.push((channel.key, channel.data_type));
                }
            }
        }
        for (index, _) in &groups {
            match crate::locate(state, *index).await.map_err(Error::Mesh)? {
                Located::Here => {}
                Located::Remote(home) => return Err(Error::Remote { home }),
            }
        }
        let mut borrowed = state.borrow_mut();
        let borrowed = &mut *borrowed;
        let groups: Vec<_> = groups
            .iter()
            .map(|(index, data)| Group {
                index: *index,
                data,
            })
            .collect();
        let set = borrowed.interner.intern(&groups);
        let writer = ::home::writer::Writer {
            subject,
            authority,
            lease,
            set: Arc::clone(&set),
        };
        let key = borrowed.home.open_writer(writer).map_err(Error::Home)?;
        borrowed.commit.appended();
        Ok(Self {
            state: Rc::clone(state),
            key,
            set,
            outcomes: Vec::new(),
        })
    }

    /// The key set of every frame the writer writes.
    #[must_use]
    pub fn set(&self) -> &Arc<KeySet> {
        &self.set
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
    /// No seq moves for any error. [`Error::Resend`](crate::home::Error::Resend) for a
    /// frame labeled resend. [`Error::Full`](crate::home::Error::Full) for a backfill
    /// frame with no room: write it again on a timer.
    /// [`Error::Large`](crate::home::Error::Large) for a frame too large for one write:
    /// split it. [`Error::Disk`](crate::home::Error::Disk) after a failed commit: the
    /// shard takes no more frames.
    ///
    /// # Panics
    ///
    /// If `frame` is not of [`Self::set`] and is not labeled resend.
    pub fn write(
        &mut self,
        label: Label,
        frame: Draft,
    ) -> Result<&[::home::Outcome], ::home::Error> {
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let written = state.home.write(self.key, label, frame);
        state.commit.appended();
        let outcomes = written?;
        self.outcomes.clear();
        self.outcomes.extend_from_slice(outcomes);
        state.wake();
        Ok(&self.outcomes)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.home.close_writer(self.key);
        state.commit.appended();
    }
}

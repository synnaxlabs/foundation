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

use crate::State;

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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "no channel is named {name}"),
            Self::Home(error) => error.fmt(f),
            Self::Empty => f.write_str("a writer names at least one channel"),
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
    pub(crate) fn open(
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
        let mut borrowed = state.borrow_mut();
        let borrowed = &mut *borrowed;
        let mut groups: Vec<(channel::Key, Vec<(channel::Key, Type)>)> = Vec::new();
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
        Draft::new(&self.state.borrow().pool, &self.set, form, series)
    }

    /// Applies `frame`, as [`::home::Shard::write`] does, and wakes each reader that
    /// now has a frame to take. Does not wait.
    ///
    /// # Errors
    ///
    /// As [`::home::Shard::write`].
    ///
    /// # Panics
    ///
    /// As [`::home::Shard::write`], for a frame that is not of [`Self::set`].
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

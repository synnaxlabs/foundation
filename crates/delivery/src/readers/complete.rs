//! The keys and open results of complete sessions.

use std::fmt;

use types::channel::Slot;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Frame, Mask, View};

use crate::Position;

/// A complete session on one index. Keys are unique within one
/// [`Readers`](crate::Readers). Use a key only with the `Readers` that gave it:
/// another one, such as a restored one, takes the key as its own when it gave the
/// same number, and panics when it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub(super) u64);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A complete session that [`Readers::open`](crate::Readers::open) started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The session.
    pub key: Key,
    /// Where the session starts.
    pub position: Position,
    /// The session of the same named reader that this one took over, in either mode.
    /// It is closed.
    pub replaced: Option<super::Key>,
}

/// What a frame costs the credit of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Charge {
    /// The frame's [`Frame::charge`]: the session gets the home's frame.
    Whole,
    /// The charge of a frame of one series for each slot, in this order, that the
    /// frame holds: the frame that a remote reader builds. A slot after its first
    /// listing, or one the frame's key set lacks, adds no series.
    Places(Box<[Slot]>),
}

/// What frames cost one session, as its [`Charge`] says.
#[derive(Debug)]
pub(super) enum Cost {
    Whole,
    Places(Places),
}

#[derive(Debug)]
pub(super) struct Places {
    slots: Box<[Slot]>,
    /// The places in the key set of the last frame charged.
    held: Option<Held>,
}

/// The places of a session in one key set.
#[derive(Debug)]
struct Held {
    set: key_set::Key,
    mask: Mask,
    /// Each entry that a place names, with the first place that names it, by entry.
    entries: Vec<(usize, usize)>,
}

impl Cost {
    pub(super) fn new(charge: Charge) -> Self {
        match charge {
            Charge::Whole => Self::Whole,
            Charge::Places(slots) => Self::Places(Places { slots, held: None }),
        }
    }

    /// What `frame`, of key set `set`, costs the session. Time is O(m log(n/m)) for
    /// m places in `set` and n series in `frame`. Allocates only for the first frame
    /// of a key set.
    pub(super) fn charge(&mut self, frame: &Frame, set: &KeySet) -> u64 {
        let Self::Places(Places { slots, held }) = self else {
            return frame.charge();
        };
        let held = match held {
            Some(held) if held.set == set.key() => held,
            held => held.insert(Held::new(slots, set)),
        };
        // The body holds each series but the last in place order padded, then the last.
        let (mut series, mut others) = (0, 0);
        let mut last: Option<(usize, usize)> = None;
        let mut entries = held.entries.iter().peekable();
        View::new(frame, &held.mask)
            .bounds()
            .for_each(|(entry, bounds)| {
                while entries.next_if(|&&(held, _)| held < entry).is_some() {}
                let Some(&(_, place)) = entries.next_if(|&&(held, _)| held == entry)
                else {
                    return;
                };
                series += 1;
                let other = match last {
                    Some((at, _)) if at > place => bounds.len(),
                    _ => last
                        .replace((place, bounds.len()))
                        .map_or(0, |(_, len)| len),
                };
                others += padded(other);
            });
        frame::charge(series, others + last.map_or(0, |(_, len)| len))
    }
}

/// The bytes that a series of `len` bytes takes in a frame's body when another
/// series follows it.
fn padded(len: usize) -> usize {
    frame::ends([((), len), ((), 0)])
        .last()
        .map_or(0, |((), end)| end)
}

impl Held {
    fn new(slots: &[Slot], set: &KeySet) -> Self {
        let mut entries: Vec<_> = slots
            .iter()
            .enumerate()
            .filter_map(|(place, &slot)| Some((set.find(slot)?, place)))
            .collect();
        entries.sort_unstable();
        entries.dedup_by_key(|&mut (entry, _)| entry);
        Self {
            set: set.key(),
            mask: Mask::new(set, slots.iter().copied()),
            entries,
        }
    }
}

//! `stored` and `split`, for the bench targets only. Not a stable surface.

use std::sync::Arc;

use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{Draft, Frame};
use types::sample::Type;
use types::time::Stamp;

use crate::{split, stored};

/// Calls `stored::entry`.
pub fn entry(
    pool: &block::Pool,
    frame: &Frame,
    set: &KeySet,
    last: Stamp,
    stored_at: Stamp,
) -> Result<buffer::Entry, block::Error> {
    stored::entry(pool, frame, set, last, stored_at)
}

/// Calls `stored::read`, and gives each series' channel, type, and bytes.
pub fn read(body: &[u8]) -> impl Iterator<Item = (channel::Key, Type, &[u8])> {
    stored::read(body).map(|series| (series.channel, series.data_type, series.bytes))
}

/// The stored entry that a read from disk gives for `frame`, an index frame of `set`.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for the entry.
pub fn stored(
    pool: &block::Pool,
    frame: &Frame,
    set: &KeySet,
) -> Result<buffer::Stored, block::Error> {
    let entry =
        stored::entry(pool, frame, set, Stamp::from_nanos(7), Stamp::from_nanos(9))?;
    let body: Vec<u8> = entry
        .parts
        .into_iter()
        .flat_map(|part| part.to_vec())
        .collect();
    let mut bytes = pool.alloc(body.len())?;
    bytes.copy_from_slice(&body);
    Ok(buffer::Stored {
        first: entry.first,
        samples: entry.samples,
        stored_at: entry.stored_at,
        tag: entry.tag,
        bytes: bytes.freeze(),
    })
}

/// Makes the frames of stored data entries again, as a read from disk of a shard does.
#[derive(Debug, Default)]
pub struct Decoder(stored::Decoder);

impl Decoder {
    /// The live frame of key set `set` that `stored` holds, an entry of the index of
    /// the one group of `set`. It drops each series whose channel and type `set` does
    /// not hold. `None` when `stored` is not a data entry with samples. Each `set`
    /// given to one decoder comes from one interner.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when `pool` has no block for the frame.
    ///
    /// # Panics
    ///
    /// If `stored` is not a body that [`stored`] made, or `set` does not hold its
    /// index series.
    pub fn frame(
        &mut self,
        pool: &block::Pool,
        set: &Arc<KeySet>,
        stored: &buffer::Stored,
    ) -> Result<Option<Frame>, block::Error> {
        self.0.frame(pool, set, stored)
    }
}

/// The buffers and encoders of [`split`], which a shard keeps from one write to the
/// next.
#[derive(Debug)]
pub struct Scratch {
    split: split::Scratch,
    encoders: Box<[codec::Encoder]>,
    groups: Vec<u32>,
}

impl Scratch {
    /// Empty buffers, and an encoder for each entry of `set`.
    #[must_use]
    pub fn new(set: &KeySet) -> Self {
        Self {
            split: split::Scratch::default(),
            encoders: split::encoders(set),
            groups: Vec::new(),
        }
    }
}

/// Checks `draft`, a writer's frame of `set`, and splits it: it encodes each raw
/// series, walks every stamp of each group's index, then makes the index frame of each
/// group and gives it to `index`. Returns the number of stamps walked.
///
/// # Panics
///
/// If `draft` is not of `set`, if a series does not fit its group's count, or if
/// `pool` has no block for an index frame.
pub fn split(
    scratch: &mut Scratch,
    pool: &block::Pool,
    set: &KeySet,
    draft: Draft,
    mut index: impl FnMut(Draft),
) -> usize {
    let mut split = scratch.split.split(set, &mut scratch.encoders, draft);
    scratch.groups.clear();
    let mut walked = 0;
    while let Some((group, stamps)) = split.next() {
        let mut stamps = stamps.expect("each series fits its group's count");
        while let Some(vector) = stamps.next() {
            walked += vector.expect("an index that decodes").len();
        }
        scratch.groups.push(group);
    }
    for &group in &scratch.groups {
        index(split.frame(pool, group).expect("room for an index frame"));
    }
    walked
}

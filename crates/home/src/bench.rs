//! `stored`, for the bench target only. Not a stable surface.

use types::channel;
use types::frame::Frame;
use types::frame::key_set::KeySet;
use types::sample::Type;
use types::time::Stamp;

use crate::stored;

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

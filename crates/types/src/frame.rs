//! Frames and the key sets they point at.

pub mod key_set;

/// One of an index's two write paths, each with its own seq. Backfill is late data,
/// labeled by the writer, that live readers never see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    /// The newest data.
    Live,
    /// Late data, labeled by the writer. It ends before the newest live sample.
    Backfill,
}

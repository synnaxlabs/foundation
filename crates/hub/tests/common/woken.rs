//! A home shard with indexes that each have one data channel and one complete reader,
//! and one writer on all of them, for the counts and times of `home::Shard::woken`.

use std::sync::Arc;

use env::tasks::Tasks;
use home::Outcome;
use home::reader::complete::Charge;
use types::authority::Authority;
use types::channel;
use types::frame::key_set::{Group, KeySet};
use types::frame::{Draft, Form, Label, Path};
use types::sample::{Scalar, Type};

use crate::shard::{name, shard};

/// The shard, its readers, and the keys of `woken`.
pub(crate) struct Woken {
    pub(crate) shard: home::Shard,
    /// The complete reader of each index, in group order.
    pub(crate) readers: Vec<home::reader::Key>,
    /// The keys that the caller gives `woken`.
    pub(crate) keys: Vec<home::reader::Key>,
    set: Arc<KeySet>,
    writer: home::writer::Key,
    /// Each series of a frame on every index.
    series: Vec<(usize, usize)>,
    stamp: i64,
}

impl Woken {
    /// A shard with `indexes` indexes. Each reader has unlimited credit.
    pub(crate) async fn new(
        node: &sim::node::Node,
        tasks: Tasks,
        indexes: usize,
    ) -> Self {
        let (mut shard, mut interner, stamp) = shard(node, tasks).await;
        let key = |n| channel::Key::from_u128(u128::try_from(n).expect("few"));
        let channels: Vec<_> = (0..indexes)
            .map(|n| {
                (
                    key(2 * n + 1),
                    [(key(2 * n + 2), Type::Scalar(Scalar::I64))],
                )
            })
            .collect();
        let groups: Vec<_> = channels
            .iter()
            .map(|(index, data)| Group {
                index: *index,
                data,
            })
            .collect();
        let set = interner.intern(&groups);
        let mut readers = Vec::with_capacity(indexes);
        for &entry in set.groups() {
            let slot = set.entries()[entry].slot;
            shard.carry(slot);
            readers.push(shard.open_complete(slot, u64::MAX, Charge::Whole).into());
        }
        let writer = shard
            .open_writer(home::writer::Writer {
                subject: name("a"),
                authority: Authority(1),
                lease: None,
                set: Arc::clone(&set),
            })
            .expect("opens");
        let series = (0..set.entries().len()).map(|entry| (entry, 8)).collect();
        Self {
            shard,
            readers,
            keys: Vec::new(),
            set,
            writer,
            series,
            stamp,
        }
    }

    /// Writes one sample on every channel, calls `woken` as a write needs, and waits
    /// for the commit. The caller then calls `woken` with [`Self::keys`].
    ///
    /// # Panics
    ///
    /// When the home does not apply each index of the frame, or the write wakes a
    /// reader.
    pub(crate) async fn commit(&mut self) {
        let mut draft =
            Draft::new(self.shard.pool(), &self.set, Form::Raw, &self.series)
                .expect("a frame");
        for &(entry, _) in &self.series {
            let bytes = draft.series_mut(entry).expect("the series is present");
            bytes.copy_from_slice(&self.stamp.to_le_bytes());
        }
        for group in 0..self.set.groups().len() {
            draft.set_count(u32::try_from(group).expect("few"), 1);
        }
        let outcomes = self
            .shard
            .write(self.writer, Label::Path(Path::Live), draft)
            .expect("the home takes it");
        let applied = outcomes
            .iter()
            .all(|outcome| matches!(outcome, Outcome::Applied { .. }));
        assert!(applied, "the home applies the frame: {outcomes:?}");
        self.stamp += 1;
        self.shard.woken(&mut self.keys);
        assert!(self.keys.is_empty(), "a write wakes no complete reader");
        self.shard.committed().await.expect("commits");
    }
}

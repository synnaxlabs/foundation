//! The names in the data directory: the record of the shard count, and the
//! directory of each shard's ring.

use std::path::{Path, PathBuf};

use crate::Error;

/// The prefix of the record: an empty directory `shards-<n>`. A crash leaves the
/// whole name or none, so the record has no bytes to tear.
const RECORD: &str = "shards-";
/// The prefix of the directory of each shard's ring.
const SHARD: &str = "shard-";

/// The directory of the ring of the shard on `core`.
pub(crate) fn shard(core: usize) -> PathBuf {
    PathBuf::from(format!("{SHARD}{core}"))
}

/// Records `cores` in the data directory when no count is there, and syncs the
/// record before any ring is made. Refuses a directory that records another count.
/// With no record, rings up to `shard-<k>` are a record of `k + 1`.
pub(crate) async fn claim(
    files: &env::files::Files,
    cores: usize,
) -> Result<(), Error> {
    let root = Path::new("");
    let names = files.list(root).await.map_err(Error::Directory)?;
    let mut counts: Vec<usize> = names
        .iter()
        .filter_map(|name| count(name, RECORD))
        .filter(|&k| k > 0)
        .collect();
    let recorded = !counts.is_empty();
    if !recorded {
        // No node has a shard of index `usize::MAX`, so that name is not a ring.
        let rings = names
            .iter()
            .filter_map(|name| count(name, SHARD)?.checked_add(1));
        counts.extend(rings.max());
    }
    // The smallest, so the error does not hang on the order of the list.
    let other = counts.iter().copied().filter(|&k| k != cores).min();
    if let Some(stored) = other {
        return Err(Error::Shards { stored, cores });
    }
    if !recorded {
        let record = PathBuf::from(format!("{RECORD}{cores}"));
        files.create_dir(&record).await.map_err(Error::Directory)?;
    }
    // Also when the record is there: a crash may have left it unsynced.
    files.sync_dir(root).await.map_err(Error::Directory)
}

/// The number after `prefix` in `name`, in plain decimal, so `shards-03` and
/// `shards-+3` give none.
fn count(name: &Path, prefix: &str) -> Option<usize> {
    let rest = name.to_str()?.strip_prefix(prefix)?;
    let count = rest.parse::<usize>().ok()?;
    (count.to_string() == rest).then_some(count)
}

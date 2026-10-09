//! The names in the data directory: the lock, the record of the shard count, the
//! directory of each shard's ring, the mesh's directory, and the chunk store's
//! directory. The node's name is in `name` ([`crate::name`]).

use std::path::{Path, PathBuf};

use crate::Error;

/// The file a running node holds open to write, so a second node gets `Busy`. The
/// node never removes it, so an open cannot race with a remove.
const LOCK: &str = "lock";
/// The prefix of the record: an empty directory `shards-<n>`. A crash leaves the
/// whole name or none, so the record has no bytes to tear.
const RECORD: &str = "shards-";
/// The prefix of the directory of each shard's ring.
const SHARD: &str = "shard-";
/// The name of the mesh's directory.
const MESH: &str = "mesh";
/// The name of the chunk store's directory.
const BLOB: &str = "blob";

/// The directory of the mesh of the node's region.
pub(crate) fn mesh() -> PathBuf {
    PathBuf::from(MESH)
}

/// The directory of the node's chunk store.
pub(crate) fn blob() -> PathBuf {
    PathBuf::from(BLOB)
}

/// The directory of the ring of the shard on `core`.
pub(crate) fn shard(core: usize) -> PathBuf {
    PathBuf::from(format!("{SHARD}{core}"))
}

/// Locks the data directory, then records `cores` in it when no count is there, and
/// syncs the record before any ring is made. Refuses a directory that records
/// another count. With no record, rings up to `shard-<k>` are a record of `k + 1`.
/// Reads names only, so a file named as a record or a ring counts as one. Then keeps
/// `name` in the file `name` ([`crate::name::keep`]). Gives the lock, which keeps out
/// other nodes until it drops.
pub(crate) async fn claim(
    files: &env::files::Files,
    cores: usize,
    name: &types::name::Name,
) -> Result<env::files::File, Error> {
    let lock = files
        .open(Path::new(LOCK), env::files::Mode::Create { len: 0 })
        .await
        .map_err(Error::Directory)?;
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
    files.sync_dir(root).await.map_err(Error::Directory)?;
    crate::name::keep(files, name).await?;
    Ok(lock)
}

/// The number after `prefix` in `name`, in plain decimal that fits a `usize`, so
/// `shards-03`, `shards-+3`, and `shards-18446744073709551616` give none.
fn count(name: &Path, prefix: &str) -> Option<usize> {
    let rest = name.to_str()?.strip_prefix(prefix)?;
    let count = rest.parse::<usize>().ok()?;
    (count.to_string() == rest).then_some(count)
}

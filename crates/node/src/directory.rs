//! The record of the shard count in the data directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use types::frame::key_set::Interner;

use crate::Error;
use crate::handoff::Give;

/// The prefix of the record: an empty directory `shards-<n>`. A crash leaves the
/// whole name or none, so the record has no bytes to tear.
const RECORD: &str = "shards-";

/// The claim of the data directory for a node of `cores` shards. It heads the
/// interner handoff, so no shard opens its ring before the claim ends.
pub(crate) struct Claim {
    pub(crate) files: Arc<dyn Fn() -> env::files::Files + Send + Sync>,
    pub(crate) cores: usize,
    pub(crate) give: Give<Interner>,
    pub(crate) failed: Arc<OnceLock<Error>>,
}

impl Claim {
    /// Claims the data directory, then gives the node's interner to shard 0. A failed
    /// claim is kept for [`crate::Node::join`] and gives none, so no ring opens.
    pub(crate) async fn run(self) {
        match claim(&(self.files)(), self.cores).await {
            Ok(()) => self.give.give(Interner::new()),
            Err(error) => self
                .failed
                .set(error)
                .expect("invariant: a failed claim stops shard 0 before its open"),
        }
    }
}

/// Records `cores` in the data directory when no count is there, and syncs it
/// before any ring is made. Refuses a directory that records another count.
async fn claim(files: &env::files::Files, cores: usize) -> Result<(), Error> {
    let root = Path::new("");
    let names = files.list(root).await.map_err(Error::Directory)?;
    let mut stored = names.iter().filter_map(|name| {
        let count = name.to_str()?.strip_prefix(RECORD)?;
        count.parse::<usize>().ok()
    });
    match stored.next() {
        Some(stored) if stored != cores => Err(Error::Shards { stored, cores }),
        Some(_) => Ok(()),
        None => {
            let record = PathBuf::from(format!("{RECORD}{cores}"));
            files.create_dir(&record).await.map_err(Error::Directory)?;
            files.sync_dir(root).await.map_err(Error::Directory)
        }
    }
}

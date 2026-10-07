use types::channel::Slot;
use types::frame::Path;

use crate::log::{Logs, Mark};

pub(crate) fn lookup(logs: &Logs, slot: Slot, path: Path, from: Mark) -> Option<u64> {
    logs.run(slot, path, from).map(|(_, run)| run.offset)
}

use types::channel::Slot;
use types::frame::Path;

use crate::log::{Found, Logs, Mark};

pub(crate) fn lookup(logs: &Logs, slot: Slot, path: Path, from: Mark) -> Option<u64> {
    match logs.find(slot, path, from) {
        Found::Run(_, run) => Some(run.offset),
        Found::End(end) => Some(end.seq),
    }
}
